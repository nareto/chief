use anyhow::{Context, anyhow};
use chief::flow::{SuiteCommandKind, configure_process_group, terminate_process_tree};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::error;

use crate::api::types::{RunSuiteCheckResponse, RunSuiteCheckStreamEvent, SuiteCheckOutputStream};

enum SuiteStreamChunk {
    Chunk {
        stream: SuiteCheckOutputStream,
        text: String,
    },
    Done {
        stream: SuiteCheckOutputStream,
    },
    Error {
        stream: SuiteCheckOutputStream,
        message: String,
    },
}

pub(crate) fn execute_suite_command_streaming<F>(
    suite_name: &str,
    kind: SuiteCommandKind,
    command: &str,
    cwd: &Path,
    cwd_display: &str,
    env: &BTreeMap<String, String>,
    timeout_seconds: u64,
    cancel_signal: Option<&Arc<AtomicBool>>,
    mut on_chunk: F,
) -> anyhow::Result<RunSuiteCheckResponse>
where
    F: FnMut(SuiteCheckOutputStream, &str),
{
    let mut process = Command::new("sh");
    process.arg("-lc").arg(command);
    process.current_dir(cwd);
    process.envs(env.iter());
    process.stdout(Stdio::piped());
    process.stderr(Stdio::piped());
    configure_process_group(&mut process);
    let mut child = process
        .spawn()
        .with_context(|| format!("failed to run command: {command}"))?;

    let (chunk_sender, chunk_receiver) = mpsc::channel::<SuiteStreamChunk>();
    let stdout_reader = spawn_suite_stream_reader(
        child.stdout.take(),
        SuiteCheckOutputStream::Stdout,
        chunk_sender.clone(),
    );
    let stderr_reader = spawn_suite_stream_reader(
        child.stderr.take(),
        SuiteCheckOutputStream::Stderr,
        chunk_sender,
    );

    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut merged_output = String::new();
    let mut read_error: Option<String> = None;
    let timeout = Duration::from_secs(timeout_seconds.max(1));
    let started = Instant::now();
    let mut timed_out = false;
    let mut cancelled_by_user = false;

    while !(stdout_done && stderr_done) {
        if cancel_signal.is_some_and(|signal| signal.load(std::sync::atomic::Ordering::SeqCst)) {
            cancelled_by_user = true;
            break;
        }

        let chunk = match chunk_receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => chunk,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel_signal
                    .is_some_and(|signal| signal.load(std::sync::atomic::Ordering::SeqCst))
                {
                    cancelled_by_user = true;
                    break;
                }
                if started.elapsed() >= timeout {
                    timed_out = true;
                    break;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("suite command output stream disconnected"));
            }
        };

        match chunk {
            SuiteStreamChunk::Chunk { stream, text } => {
                match stream {
                    SuiteCheckOutputStream::Stdout => stdout.push_str(&text),
                    SuiteCheckOutputStream::Stderr => stderr.push_str(&text),
                }
                merged_output.push_str(&text);
                on_chunk(stream, &text);
            }
            SuiteStreamChunk::Done { stream } => match stream {
                SuiteCheckOutputStream::Stdout => stdout_done = true,
                SuiteCheckOutputStream::Stderr => stderr_done = true,
            },
            SuiteStreamChunk::Error { stream, message } => {
                read_error = Some(format!(
                    "failed reading {} stream: {message}",
                    suite_stream_label(stream)
                ));
                break;
            }
        }
    }

    if read_error.is_some() || timed_out || cancelled_by_user {
        terminate_process_tree(&mut child);
    }
    let status = child.wait().context("failed waiting for suite command")?;
    join_suite_stream_reader(stdout_reader, "stdout")?;
    join_suite_stream_reader(stderr_reader, "stderr")?;
    if let Some(message) = read_error {
        return Err(anyhow!(message));
    }
    if cancelled_by_user {
        return Err(anyhow!("suite command cancelled by user"));
    }

    if timed_out {
        let timeout_message = format!(
            "suite command timed out after {} second(s) and was terminated.",
            timeout_seconds.max(1)
        );
        merged_output = if merged_output.trim().is_empty() {
            timeout_message.clone()
        } else {
            format!("{timeout_message}\n{}", merged_output.trim())
        };
        if !stderr.contains(&timeout_message) {
            if stderr.trim().is_empty() {
                stderr = timeout_message;
            } else {
                stderr = format!("{stderr}\n{timeout_message}");
            }
        }
    }

    Ok(RunSuiteCheckResponse {
        suite: suite_name.to_owned(),
        kind,
        command: command.to_owned(),
        cwd: cwd_display.to_owned(),
        exit_code: if timed_out {
            124
        } else {
            status.code().unwrap_or(1)
        },
        output: merged_output.trim().to_owned(),
        stdout,
        stderr,
    })
}

fn spawn_suite_stream_reader<T>(
    pipe: Option<T>,
    stream: SuiteCheckOutputStream,
    sender: mpsc::Sender<SuiteStreamChunk>,
) -> JoinHandle<anyhow::Result<()>>
where
    T: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let Some(pipe) = pipe else {
            let _ = sender.send(SuiteStreamChunk::Done { stream });
            return Ok(());
        };

        let mut reader = BufReader::new(pipe);
        loop {
            let mut bytes = Vec::new();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&bytes).to_string();
                    if sender
                        .send(SuiteStreamChunk::Chunk { stream, text })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(err) => {
                    let _ = sender.send(SuiteStreamChunk::Error {
                        stream,
                        message: err.to_string(),
                    });
                    return Ok(());
                }
            }
        }

        let _ = sender.send(SuiteStreamChunk::Done { stream });
        Ok(())
    })
}

fn join_suite_stream_reader(
    handle: JoinHandle<anyhow::Result<()>>,
    stream_name: &str,
) -> anyhow::Result<()> {
    match handle.join() {
        Ok(result) => result.with_context(|| format!("failed reading {stream_name} stream")),
        Err(_) => Err(anyhow!("{stream_name} stream reader thread panicked")),
    }
}

fn suite_stream_label(stream: SuiteCheckOutputStream) -> &'static str {
    match stream {
        SuiteCheckOutputStream::Stdout => "stdout",
        SuiteCheckOutputStream::Stderr => "stderr",
    }
}

pub(crate) fn send_stream_event_blocking(
    sender: &tokio_mpsc::Sender<Vec<u8>>,
    event: RunSuiteCheckStreamEvent,
) -> bool {
    let payload = match stream_event_payload(&event) {
        Ok(payload) => payload,
        Err(err) => {
            error!(error = %err, "failed to encode suite stream event");
            return false;
        }
    };
    sender.blocking_send(payload).is_ok()
}

pub(crate) async fn send_stream_event_async(
    sender: &tokio_mpsc::Sender<Vec<u8>>,
    event: RunSuiteCheckStreamEvent,
) -> bool {
    let payload = match stream_event_payload(&event) {
        Ok(payload) => payload,
        Err(err) => {
            error!(error = %err, "failed to encode suite stream event");
            return false;
        }
    };
    sender.send(payload).await.is_ok()
}

fn stream_event_payload(event: &RunSuiteCheckStreamEvent) -> anyhow::Result<Vec<u8>> {
    let mut payload = serde_json::to_vec(event).context("failed serializing suite stream event")?;
    payload.push(b'\n');
    Ok(payload)
}
