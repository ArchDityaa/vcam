use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use vcam_core::ipc::{Request, Response, ServerMessage};
use vcam_core::Event;

use crate::controller::DaemonRequest;

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub async fn serve(
    socket_path: PathBuf,
    command_tx: mpsc::UnboundedSender<DaemonRequest>,
    events: broadcast::Sender<Event>,
    ready: oneshot::Sender<Result<(), String>>,
    shutdown: Arc<Notify>,
) {
    if socket_path.exists() {
        if let Err(error) = std::fs::remove_file(&socket_path) {
            let _ = ready.send(Err(format!(
                "failed to remove stale socket {}: {error}",
                socket_path.display()
            )));
            return;
        }
    }
    if let Some(parent) = socket_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            let _ = ready.send(Err(format!(
                "failed to create socket directory {}: {error}",
                parent.display()
            )));
            return;
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            let _ = ready.send(Err(format!(
                "failed to bind {}: {error}",
                socket_path.display()
            )));
            return;
        }
    };

    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600));
    }

    let _guard = SocketGuard(socket_path.clone());
    let _ = ready.send(Ok(()));
    tracing::info!(socket = %socket_path.display(), "control socket ready");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _address)) => {
                        let command_tx = command_tx.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle_client(stream, command_tx, events).await {
                                tracing::debug!(%error, "client disconnected with error");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to accept control connection");
                    }
                }
            }
            _ = shutdown.notified() => {
                tracing::info!("control server stopping");
                break;
            }
        }
    }
}

async fn handle_client(
    stream: UnixStream,
    command_tx: mpsc::UnboundedSender<DaemonRequest>,
    events: broadcast::Sender<Event>,
) -> Result<(), String> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let mut event_rx = events.subscribe();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let request = match serde_json::from_str::<Request>(&line) {
                            Ok(request) => request,
                            Err(error) => {
                                write_json(&mut write_half, &ServerMessage::Response(
                                    Response::err(0, format!("invalid request: {error}"))
                                )).await?;
                                continue;
                            }
                        };
                        let (reply_tx, reply_rx) = oneshot::channel();
                        let daemon_request = DaemonRequest {
                            id: request.id,
                            command: request.command,
                            reply: reply_tx,
                        };
                        if command_tx.send(daemon_request).is_err() {
                            write_json(&mut write_half, &ServerMessage::Response(
                                Response::err(request.id, "daemon is shutting down")
                            )).await?;
                            break;
                        }
                        match tokio::time::timeout(Duration::from_secs(20), reply_rx).await {
                            Ok(Ok(response)) => {
                                write_json(&mut write_half, &ServerMessage::Response(response)).await?;
                            }
                            Ok(Err(_)) => {
                                write_json(&mut write_half, &ServerMessage::Response(
                                    Response::err(request.id, "daemon dropped the request")
                                )).await?;
                            }
                            Err(_) => {
                                write_json(&mut write_half, &ServerMessage::Response(
                                    Response::err(request.id, "request timed out")
                                )).await?;
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::debug!(%error, "failed to read control request");
                        break;
                    }
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(event) => {
                        write_json(&mut write_half, &ServerMessage::Event(event)).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    Ok(())
}

async fn write_json<T: Serialize>(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    message: &T,
) -> Result<(), String> {
    let mut text = serde_json::to_string(message)
        .map_err(|error| format!("failed to encode response: {error}"))?;
    text.push('\n');
    writer
        .write_all(text.as_bytes())
        .await
        .map_err(|error| format!("failed to write response: {error}"))?;
    Ok(())
}
