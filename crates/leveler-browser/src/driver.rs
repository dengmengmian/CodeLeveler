//! The transport to the Node/Playwright driver subprocess.
//!
//! Newline-delimited JSON-RPC 2.0 over the child's stdin/stdout — the same
//! shape the MCP stdio client uses. The child writes exactly one JSON message
//! per line to stdout (protocol) and everything else to stderr (diagnostics);
//! a reader task routes id-correlated responses to waiters and pushes
//! `event` notifications onto a broadcast channel. The child is `kill_on_drop`,
//! so the driver dies with the runtime that owns it.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, broadcast, oneshot};

use crate::BrowserError;

/// An observable event pushed by the driver (console/page errors, dialogs).
#[derive(Debug, Clone)]
pub struct DriverEvent {
    /// `console` | `pageerror` | `dialog`.
    pub kind: String,
    pub page: String,
    /// Free-form payload (message/level/type) as reported by the driver.
    pub payload: Value,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, BrowserError>>>>>;

/// A live driver subprocess speaking JSON-RPC over stdio.
pub struct BrowserDriver {
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Pending,
    events: broadcast::Sender<DriverEvent>,
    // Held so the child is SIGKILLed on drop (kill_on_drop).
    _child: tokio::process::Child,
}

impl BrowserDriver {
    /// Spawn `node <script>` with the given working directory and environment.
    /// The env is passed explicitly (already scrubbed by the caller) — the
    /// child does not inherit the ambient process environment.
    pub async fn spawn(
        node: &std::path::Path,
        script: &std::path::Path,
        cwd: &std::path::Path,
        envs: &[(String, String)],
    ) -> Result<Self, BrowserError> {
        let mut cmd = Command::new(node);
        cmd.arg(script)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env_clear();
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| BrowserError::LaunchFailed(format!("spawn driver: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BrowserError::LaunchFailed("driver stdin missing".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BrowserError::LaunchFailed("driver stdout missing".into()))?;
        let stderr = child.stderr.take();

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, _) = broadcast::channel(256);

        // Reader task: route responses to waiters, events to the broadcast.
        {
            let pending = pending.clone();
            let events = events_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if msg.get("method").and_then(Value::as_str) == Some("event") {
                        let p = &msg["params"];
                        let _ = events.send(DriverEvent {
                            kind: p
                                .get("kind")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            page: p
                                .get("pageId")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            payload: p.clone(),
                        });
                        continue;
                    }
                    let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    let waiter = pending.lock().await.remove(&id);
                    if let Some(tx) = waiter {
                        let outcome = if let Some(err) = msg.get("error") {
                            Err(map_driver_error(err))
                        } else {
                            Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = tx.send(outcome);
                    }
                }
                // stdout closed → driver gone. Fail every outstanding waiter.
                let mut map = pending.lock().await;
                for (_, tx) in map.drain() {
                    let _ = tx.send(Err(BrowserError::DriverDisconnected(
                        "driver stdout closed".into(),
                    )));
                }
            });
        }
        // Drain stderr to the tracing log (diagnostics only, never protocol).
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "browser_driver", "{line}");
                }
            });
        }

        Ok(Self {
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending,
            events: events_tx,
            _child: child,
        })
    }

    /// Subscribe to driver events (console/page errors/dialogs).
    pub fn subscribe(&self) -> broadcast::Receiver<DriverEvent> {
        self.events.subscribe()
    }

    /// Issue one request and await its response, bounded by `timeout`.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, BrowserError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string();
        {
            let mut stdin = self.stdin.lock().await;
            if let Err(e) = async {
                stdin.write_all(line.as_bytes()).await?;
                stdin.write_all(b"\n").await?;
                stdin.flush().await
            }
            .await
            {
                self.pending.lock().await.remove(&id);
                return Err(BrowserError::DriverDisconnected(format!("write: {e}")));
            }
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(BrowserError::DriverDisconnected(
                "driver dropped response".into(),
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(BrowserError::ActionTimeout {
                    stage: method.to_string(),
                    message: format!("no driver response within {}ms", timeout.as_millis()),
                })
            }
        }
    }
}

/// Map a driver `error` object (`{code,message,kind}`) to a typed error.
fn map_driver_error(err: &Value) -> BrowserError {
    let msg = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("driver error")
        .to_string();
    match err
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("action_failed")
    {
        "ref_stale" => BrowserError::RefStale(msg),
        "action_timeout" => BrowserError::ActionTimeout {
            stage: "action".into(),
            message: msg,
        },
        "page_closed" => BrowserError::PageClosed(msg),
        "launch_failed" => BrowserError::LaunchFailed(msg),
        "denied" => BrowserError::Denied(msg),
        _ => BrowserError::ActionFailed(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_error_kinds_map_to_typed_errors() {
        assert!(matches!(
            map_driver_error(&json!({"kind":"ref_stale","message":"gone"})),
            BrowserError::RefStale(_)
        ));
        assert!(matches!(
            map_driver_error(&json!({"kind":"page_closed","message":"x"})),
            BrowserError::PageClosed(_)
        ));
        assert!(matches!(
            map_driver_error(&json!({"kind":"denied","message":"SSRF policy blocked"})),
            BrowserError::Denied(_)
        ));
        assert!(matches!(
            map_driver_error(&json!({"message":"x"})),
            BrowserError::ActionFailed(_)
        ));
    }
}
