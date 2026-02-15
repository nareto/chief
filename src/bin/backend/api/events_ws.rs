use crate::api::error::ApiError;
use crate::api::service::ReadinessStreamMessage;
use crate::app::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use chief::domain::EventRecord;
use chief::storage::ProjectStore;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast::Receiver as BroadcastReceiver;
use tokio::time::{Duration, sleep};
use tracing::warn;

const SNAPSHOT_BATCH_SIZE: usize = 500;
const INCREMENTAL_BATCH_SIZE: usize = 250;
const STREAM_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum EventsWsMessage {
    Snapshot { events: Vec<EventRecord> },
    Event { event: EventRecord },
    ReadinessStreamReset,
    ReadinessStreamChunk { text: String },
    Error { message: String },
}

pub async fn events_ws(
    project: String,
    state: Arc<AppState>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let store = state.service.project_store_for_events(&project).await?;
    let readiness_receiver = state.service.subscribe_readiness_stream();
    let readiness_snapshot = state.service.readiness_stream_snapshot(&project);
    Ok(ws.on_upgrade(move |socket| {
        handle_events_socket(
            socket,
            project,
            store,
            readiness_receiver,
            readiness_snapshot,
        )
    }))
}

async fn handle_events_socket(
    socket: WebSocket,
    project: String,
    store: ProjectStore,
    mut readiness_receiver: BroadcastReceiver<ReadinessStreamMessage>,
    readiness_snapshot: Option<String>,
) {
    let (mut sender, mut receiver) = socket.split();

    let mut last_id = match collect_snapshot(&store) {
        Ok((snapshot, newest_id)) => {
            if send_message(&mut sender, EventsWsMessage::Snapshot { events: snapshot })
                .await
                .is_err()
            {
                return;
            }
            newest_id
        }
        Err(err) => {
            let _ = send_message(
                &mut sender,
                EventsWsMessage::Error {
                    message: format!("failed to load events snapshot: {err}"),
                },
            )
            .await;
            return;
        }
    };

    if let Some(snapshot) = readiness_snapshot {
        if send_message(&mut sender, EventsWsMessage::ReadinessStreamReset)
            .await
            .is_err()
        {
            return;
        }
        if send_message(
            &mut sender,
            EventsWsMessage::ReadinessStreamChunk { text: snapshot },
        )
        .await
        .is_err()
        {
            return;
        }
    }

    'stream: loop {
        tokio::select! {
            maybe_message = receiver.next() => {
                match maybe_message {
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break 'stream;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break 'stream,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break 'stream,
                }
            }
            readiness_message = readiness_receiver.recv() => {
                match readiness_message {
                    Ok(ReadinessStreamMessage::Reset { project: stream_project }) => {
                        if stream_project != project {
                            continue;
                        }
                        if send_message(&mut sender, EventsWsMessage::ReadinessStreamReset)
                            .await
                            .is_err()
                        {
                            break 'stream;
                        }
                    }
                    Ok(ReadinessStreamMessage::Chunk { project: stream_project, text }) => {
                        if stream_project != project {
                            continue;
                        }
                        if send_message(&mut sender, EventsWsMessage::ReadinessStreamChunk { text })
                            .await
                            .is_err()
                        {
                            break 'stream;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(
                            project = %project,
                            skipped,
                            "events stream readiness channel lagged"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break 'stream,
                }
            }
            _ = sleep(STREAM_POLL_INTERVAL) => {
                match store.query_events_after_id(last_id, INCREMENTAL_BATCH_SIZE) {
                    Ok(events) => {
                        for event in events {
                            if let Some(event_id) = event.id {
                                last_id = last_id.max(event_id);
                            }
                            if send_message(&mut sender, EventsWsMessage::Event { event })
                                .await
                                .is_err()
                            {
                                break 'stream;
                            }
                        }
                    }
                    Err(err) => {
                        warn!(project = %project, error = %err, "events stream polling failed");
                        let _ = send_message(
                            &mut sender,
                            EventsWsMessage::Error {
                                message: format!("failed to fetch incremental events: {err}"),
                            },
                        )
                        .await;
                        break 'stream;
                    }
                }
            }
        }
    }
}

fn collect_snapshot(store: &ProjectStore) -> anyhow::Result<(Vec<EventRecord>, i64)> {
    let mut after_id = 0_i64;
    let mut events_ascending = Vec::new();

    loop {
        let batch = store.query_events_after_id(after_id, SNAPSHOT_BATCH_SIZE)?;
        if batch.is_empty() {
            break;
        }

        if let Some(last) = batch.last().and_then(|event| event.id) {
            after_id = last;
        }
        events_ascending.extend(batch);
    }

    events_ascending.reverse();
    Ok((events_ascending, after_id))
}

async fn send_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    payload: EventsWsMessage,
) -> Result<(), ()> {
    let json = serde_json::to_string(&payload).map_err(|_| ())?;
    sender
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}
