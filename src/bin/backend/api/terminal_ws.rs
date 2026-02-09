use crate::api::error::ApiError;
use crate::app::AppState;
use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

pub async fn terminal_ws(
    project: String,
    state: Arc<AppState>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if !state.terminal_enabled {
        return Err(ApiError::forbidden(
            "terminal websocket is disabled; start backend with --enable-terminal",
        ));
    }

    let project_dir = state.service.project_dir_for_terminal(&project).await?;
    Ok(ws.on_upgrade(move |socket| handle_terminal(socket, project_dir)))
}

async fn handle_terminal(mut socket: WebSocket, project_dir: PathBuf) {
    let mut session = match open_pty_session(&project_dir) {
        Ok(session) => session,
        Err(err) => {
            let _ = socket
                .send(Message::Text(
                    format!("terminal unavailable: {err}\r\n").into(),
                ))
                .await;
            return;
        }
    };

    let (mut sender, mut receiver) = socket.split();
    let (output_tx, mut output_rx) = mpsc::channel::<Vec<u8>>(128);
    let mut reader = session.reader;

    let read_task = tokio::task::spawn_blocking(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if output_tx.blocking_send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = output_tx.blocking_send(
                        format!("\r\n[terminal read error: {err}]\r\n").into_bytes(),
                    );
                    break;
                }
            }
        }
    });

    loop {
        tokio::select! {
            maybe_chunk = output_rx.recv() => {
                let Some(chunk) = maybe_chunk else {
                    break;
                };
                let text = String::from_utf8_lossy(&chunk).into_owned();
                if sender.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
            maybe_message = receiver.next() => {
                match maybe_message {
                    Some(Ok(Message::Text(payload))) => {
                        if let Err(err) = handle_client_payload(
                            &payload,
                            session.writer.as_mut(),
                            session.master.as_ref(),
                        ) {
                            let _ = sender.send(Message::Text(format!("\r\n[terminal error: {err}]\r\n").into())).await;
                        }
                    }
                    Some(Ok(Message::Binary(payload))) => {
                        if let Err(err) = write_to_pty(session.writer.as_mut(), payload.as_ref()) {
                            let _ = sender.send(Message::Text(format!("\r\n[terminal error: {err}]\r\n").into())).await;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    drop(session.writer);
    drop(session.master);
    let _ = session.child.kill();
    let _ = session.child.wait();
    let _ = read_task.await;
}

struct PtySession {
    child: Box<dyn Child + Send>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    reader: Box<dyn Read + Send>,
}

fn open_pty_session(project_dir: &Path) -> Result<PtySession> {
    let pty_system = native_pty_system();
    let pty_pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to allocate pty")?;

    let shell = resolve_shell();
    let mut command = CommandBuilder::new(shell);
    command.cwd(project_dir);
    command.env("TERM", "xterm-256color");

    let child = pty_pair
        .slave
        .spawn_command(command)
        .context("failed to spawn shell in pty")?;
    drop(pty_pair.slave);

    let writer = pty_pair
        .master
        .take_writer()
        .context("failed to create pty writer")?;
    let reader = pty_pair
        .master
        .try_clone_reader()
        .context("failed to create pty reader")?;

    Ok(PtySession {
        child,
        master: pty_pair.master,
        writer,
        reader,
    })
}

fn resolve_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "/bin/bash".to_owned())
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TerminalClientPayload {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

fn handle_client_payload(
    payload: &str,
    writer: &mut (dyn Write + Send),
    master: &(dyn MasterPty + Send),
) -> Result<()> {
    if payload.is_empty() {
        return Ok(());
    }

    if let Ok(message) = serde_json::from_str::<TerminalClientPayload>(payload) {
        match message {
            TerminalClientPayload::Input { data } => {
                if data.is_empty() {
                    return Ok(());
                }
                write_to_pty(writer, data.as_bytes())
            }
            TerminalClientPayload::Resize { cols, rows } => {
                let size = PtySize {
                    rows: rows.max(1),
                    cols: cols.max(1),
                    pixel_width: 0,
                    pixel_height: 0,
                };
                master.resize(size).context("failed to resize pty")
            }
        }
    } else {
        write_to_pty(writer, payload.as_bytes())
    }
}

fn write_to_pty(writer: &mut (dyn Write + Send), bytes: &[u8]) -> Result<()> {
    writer
        .write_all(bytes)
        .context("failed writing input to pty")?;
    writer.flush().context("failed flushing pty input")
}
