use crate::agent::AgentOutputStream;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use tokio::sync::broadcast;

const AGENT_QUERY_STREAM_CHANNEL_CAPACITY: usize = 1024;
const AGENT_QUERY_STREAM_MAX_BUFFER_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub enum AgentQueryStreamMessage {
    Started {
        project: String,
        query_id: String,
        run_id: String,
        job_id: String,
        todo_id: String,
        phase: String,
    },
    Chunk {
        project: String,
        query_id: String,
        stream: AgentOutputStream,
        text: String,
    },
    Completed {
        project: String,
        query_id: String,
        exit_code: Option<i32>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct AgentQueryStreamSnapshot {
    pub query_id: String,
    pub run_id: String,
    pub job_id: String,
    pub todo_id: String,
    pub phase: String,
    pub output: String,
}

#[derive(Debug, Clone)]
struct ActiveAgentQueryStream {
    run_id: String,
    job_id: String,
    todo_id: String,
    phase: String,
    output: String,
}

#[derive(Debug)]
struct AgentQueryStreamBus {
    sender: broadcast::Sender<AgentQueryStreamMessage>,
    active_by_project: Mutex<HashMap<String, HashMap<String, ActiveAgentQueryStream>>>,
}

static AGENT_QUERY_STREAM_BUS: OnceLock<AgentQueryStreamBus> = OnceLock::new();

fn stream_bus() -> &'static AgentQueryStreamBus {
    AGENT_QUERY_STREAM_BUS.get_or_init(|| {
        let (sender, _) = broadcast::channel(AGENT_QUERY_STREAM_CHANNEL_CAPACITY);
        AgentQueryStreamBus {
            sender,
            active_by_project: Mutex::new(HashMap::new()),
        }
    })
}

pub fn subscribe() -> broadcast::Receiver<AgentQueryStreamMessage> {
    stream_bus().sender.subscribe()
}

pub fn start_query(
    project: &str,
    query_id: &str,
    run_id: &str,
    job_id: &str,
    todo_id: &str,
    phase: &str,
) {
    if let Ok(mut active_by_project) = stream_bus().active_by_project.lock() {
        let active_for_project = active_by_project.entry(project.to_owned()).or_default();
        active_for_project.insert(
            query_id.to_owned(),
            ActiveAgentQueryStream {
                run_id: run_id.to_owned(),
                job_id: job_id.to_owned(),
                todo_id: todo_id.to_owned(),
                phase: phase.to_owned(),
                output: String::new(),
            },
        );
    }

    let _ = stream_bus().sender.send(AgentQueryStreamMessage::Started {
        project: project.to_owned(),
        query_id: query_id.to_owned(),
        run_id: run_id.to_owned(),
        job_id: job_id.to_owned(),
        todo_id: todo_id.to_owned(),
        phase: phase.to_owned(),
    });
}

pub fn push_chunk(project: &str, query_id: &str, stream: AgentOutputStream, text: &str) {
    if text.is_empty() {
        return;
    }

    if let Ok(mut active_by_project) = stream_bus().active_by_project.lock()
        && let Some(active_for_project) = active_by_project.get_mut(project)
        && let Some(active_query) = active_for_project.get_mut(query_id)
    {
        active_query.output.push_str(text);
        trim_leading_bytes(
            &mut active_query.output,
            AGENT_QUERY_STREAM_MAX_BUFFER_BYTES,
        );
    }

    let _ = stream_bus().sender.send(AgentQueryStreamMessage::Chunk {
        project: project.to_owned(),
        query_id: query_id.to_owned(),
        stream,
        text: text.to_owned(),
    });
}

pub fn complete_query(
    project: &str,
    query_id: &str,
    exit_code: Option<i32>,
    error: Option<String>,
) {
    if let Ok(mut active_by_project) = stream_bus().active_by_project.lock()
        && let Some(active_for_project) = active_by_project.get_mut(project)
    {
        active_for_project.remove(query_id);
        if active_for_project.is_empty() {
            active_by_project.remove(project);
        }
    }

    let _ = stream_bus()
        .sender
        .send(AgentQueryStreamMessage::Completed {
            project: project.to_owned(),
            query_id: query_id.to_owned(),
            exit_code,
            error,
        });
}

pub fn snapshot_for_project(project: &str) -> Vec<AgentQueryStreamSnapshot> {
    let Ok(active_by_project) = stream_bus().active_by_project.lock() else {
        return Vec::new();
    };

    let mut snapshots = active_by_project
        .get(project)
        .into_iter()
        .flat_map(|active_for_project| {
            active_for_project
                .iter()
                .map(|(query_id, query)| AgentQueryStreamSnapshot {
                    query_id: query_id.clone(),
                    run_id: query.run_id.clone(),
                    job_id: query.job_id.clone(),
                    todo_id: query.todo_id.clone(),
                    phase: query.phase.clone(),
                    output: query.output.clone(),
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    snapshots.sort_by(|left, right| left.query_id.cmp(&right.query_id));
    snapshots
}

fn trim_leading_bytes(buffer: &mut String, max_bytes: usize) {
    if max_bytes == 0 {
        buffer.clear();
        return;
    }
    if buffer.len() <= max_bytes {
        return;
    }

    let trim_from = buffer.len() - max_bytes;
    let boundary = if buffer.is_char_boundary(trim_from) {
        trim_from
    } else {
        buffer
            .char_indices()
            .map(|(idx, _)| idx)
            .find(|idx| *idx >= trim_from)
            .unwrap_or(buffer.len())
    };
    buffer.drain(..boundary);
}
