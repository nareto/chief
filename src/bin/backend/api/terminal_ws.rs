use crate::api::error::ApiError;
use crate::app::AppState;
use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::StreamExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

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
    let _ = socket
        .send(Message::Text(
            "Connected terminal. Send one shell command per message.".into(),
        ))
        .await;

    while let Some(Ok(message)) = socket.next().await {
        match message {
            Message::Text(command) => {
                let command = command.trim();
                if command.is_empty() {
                    continue;
                }

                let result = run_terminal_command(&project_dir, command);
                let payload = match result {
                    Ok(output) => output,
                    Err(err) => format!("error: {err}"),
                };

                if socket.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

fn run_terminal_command(project_dir: &PathBuf, command: &str) -> Result<String> {
    let output = Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(project_dir)
        .output()
        .with_context(|| format!("failed to run command: {command}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(format!(
        "$ {command}\nexit_code={}\n{}{}",
        output.status.code().unwrap_or(1),
        stdout,
        stderr
    ))
}
