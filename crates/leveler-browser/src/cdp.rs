//! Chrome DevTools Protocol backend — Chrome, Edge and Chromium.
//!
//! `Rust → CDP → browser`. No Node, no Playwright, no custom RPC: the browser
//! is started with `--remote-debugging-port=0`, writes the port it chose into
//! `DevToolsActivePort` under the profile, and everything after that is CDP
//! over one WebSocket.
//!
//! Which EXECUTABLE runs is the caller's decision (`discover::availability`).
//! This module drives whatever it was handed — an Edge default launches Edge.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, oneshot};

use crate::backend::{Act, BrowserBackend, RawSnapshot, RawTab, snapshot_script};
use crate::{
    BrowserError, BrowserResult, ConsoleEntry, InspectKind, InspectReport, NetworkEntry, TabId,
};

const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const NAV_TIMEOUT: Duration = Duration::from_secs(30);
const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
/// How much observation one tab keeps. Bounded so a chatty page cannot grow
/// the daemon without limit.
const OBSERVATION_CAP: usize = 200;

// ── the connection ───────────────────────────────────────────────────────────

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, BrowserError>>>>>;

/// What one attached page target has been observed doing.
#[derive(Default)]
struct Observation {
    console: Vec<ConsoleEntry>,
    page_errors: Vec<ConsoleEntry>,
    network: Vec<NetworkEntry>,
    /// requestId → index into `network`, so a response/failure lands on its
    /// own request instead of appending a second row.
    in_flight: HashMap<String, usize>,
}

impl Observation {
    fn push_console(&mut self, entry: ConsoleEntry) {
        push_capped(&mut self.console, entry);
    }
    fn push_error(&mut self, entry: ConsoleEntry) {
        push_capped(&mut self.page_errors, entry);
    }
}

fn push_capped<T>(v: &mut Vec<T>, item: T) {
    if v.len() >= OBSERVATION_CAP {
        v.remove(0);
    }
    v.push(item);
}

struct Connection {
    tx: Mutex<futures_util::stream::SplitSink<WsStream, tungstenite::Message>>,
    next_id: AtomicU64,
    pending: Pending,
    dead: Arc<AtomicBool>,
    observations: Arc<Mutex<HashMap<String, Observation>>>,
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
use tokio_tungstenite::tungstenite;

impl Connection {
    async fn open(url: &str) -> BrowserResult<Arc<Self>> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| BrowserError::LaunchFailed(format!("CDP connect: {e}")))?;
        let (tx, mut rx) = ws.split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));
        let observations = Arc::new(Mutex::new(HashMap::new()));

        let conn = Arc::new(Self {
            tx: Mutex::new(tx),
            next_id: AtomicU64::new(1),
            pending: pending.clone(),
            dead: dead.clone(),
            observations: observations.clone(),
        });

        tokio::spawn(async move {
            while let Some(Ok(msg)) = rx.next().await {
                let text = match msg {
                    tungstenite::Message::Text(t) => t.to_string(),
                    tungstenite::Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let waiter = pending.lock().await.remove(&id);
                    if let Some(tx) = waiter {
                        let outcome = match v.get("error") {
                            Some(e) => Err(map_protocol_error(e)),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(outcome);
                    }
                    continue;
                }
                record_event(&observations, &v).await;
            }
            // The socket closed: the browser is gone. Every waiter learns it,
            // and every later call learns it without waiting for a timeout
            // (§24 — a dead connection is never reported as Ready).
            dead.store(true, Ordering::SeqCst);
            let mut map = pending.lock().await;
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(BrowserError::Disconnected(
                    "the browser closed its DevTools connection".into(),
                )));
            }
        });
        Ok(conn)
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> BrowserResult<Value> {
        if self.is_dead() {
            return Err(BrowserError::Disconnected(format!(
                "the browser exited before {method}"
            )));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let mut frame = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            frame["sessionId"] = json!(s);
        }
        {
            let mut sink = self.tx.lock().await;
            if let Err(e) = sink
                .send(tungstenite::Message::Text(frame.to_string().into()))
                .await
            {
                self.pending.lock().await.remove(&id);
                self.dead.store(true, Ordering::SeqCst);
                return Err(BrowserError::Disconnected(format!("CDP write: {e}")));
            }
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Err(BrowserError::Disconnected(
                "the browser dropped the response".into(),
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(BrowserError::Timeout {
                    stage: method.to_string(),
                    message: format!("no answer within {}ms", timeout.as_millis()),
                })
            }
        }
    }
}

/// Fold one CDP event into the observation buffer of the session it came from.
async fn record_event(observations: &Arc<Mutex<HashMap<String, Observation>>>, v: &Value) {
    let Some(session) = v.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    let method = v.get("method").and_then(Value::as_str).unwrap_or("");
    let p = v.get("params").cloned().unwrap_or(Value::Null);
    let mut map = observations.lock().await;
    let obs = map.entry(session.to_string()).or_default();
    match method {
        "Runtime.consoleAPICalled" => {
            let level = p.get("type").and_then(Value::as_str).unwrap_or("log");
            if matches!(level, "error" | "warning" | "assert") {
                obs.push_console(ConsoleEntry {
                    level: level.to_string(),
                    text: console_args_text(&p),
                });
            }
        }
        "Log.entryAdded" => {
            let entry = p.get("entry").cloned().unwrap_or(Value::Null);
            let level = entry.get("level").and_then(Value::as_str).unwrap_or("info");
            if matches!(level, "error" | "warning") {
                obs.push_console(ConsoleEntry {
                    level: level.to_string(),
                    text: entry
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
        "Runtime.exceptionThrown" => {
            let d = p.get("exceptionDetails").cloned().unwrap_or(Value::Null);
            let text = d
                .get("exception")
                .and_then(|e| e.get("description"))
                .and_then(Value::as_str)
                .or_else(|| d.get("text").and_then(Value::as_str))
                .unwrap_or("uncaught exception")
                .to_string();
            obs.push_error(ConsoleEntry {
                level: "pageerror".into(),
                text,
            });
        }
        "Network.requestWillBeSent" => {
            let Some(id) = p.get("requestId").and_then(Value::as_str) else {
                return;
            };
            let req = p.get("request").cloned().unwrap_or(Value::Null);
            if obs.network.len() >= OBSERVATION_CAP {
                obs.network.remove(0);
                for idx in obs.in_flight.values_mut() {
                    *idx = idx.saturating_sub(1);
                }
            }
            obs.network.push(NetworkEntry {
                method: req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("GET")
                    .to_string(),
                url: req
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: None,
                failure: None,
            });
            let at = obs.network.len() - 1;
            obs.in_flight.insert(id.to_string(), at);
        }
        "Network.responseReceived" => {
            let Some(id) = p.get("requestId").and_then(Value::as_str) else {
                return;
            };
            if let Some(&at) = obs.in_flight.get(id)
                && let Some(row) = obs.network.get_mut(at)
            {
                row.status = p
                    .get("response")
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_u64)
                    .map(|s| s as u32);
            }
        }
        "Network.loadingFailed" => {
            let Some(id) = p.get("requestId").and_then(Value::as_str) else {
                return;
            };
            if let Some(&at) = obs.in_flight.get(id)
                && let Some(row) = obs.network.get_mut(at)
            {
                row.failure = Some(
                    p.get("errorText")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                        .to_string(),
                );
            }
        }
        _ => {}
    }
}

fn console_args_text(p: &Value) -> String {
    let Some(args) = p.get("args").and_then(Value::as_array) else {
        return String::new();
    };
    args.iter()
        .map(|a| {
            a.get("value")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    a.get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .or_else(|| a.get("value").map(|v| v.to_string()))
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn map_protocol_error(e: &Value) -> BrowserError {
    let msg = e
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("CDP error")
        .to_string();
    let detail = e.get("data").and_then(Value::as_str).unwrap_or("");
    let full = if detail.is_empty() {
        msg
    } else {
        format!("{msg}: {detail}")
    };
    if full.contains("No node with given id") || full.contains("Could not find node") {
        BrowserError::RefStale(full)
    } else if full.contains("Target closed") || full.contains("Session with given id not found") {
        BrowserError::TabClosed(full)
    } else {
        BrowserError::ActionFailed(full)
    }
}

// ── the process ──────────────────────────────────────────────────────────────

/// The browser child, owned so the whole tree dies with the daemon (§25).
struct BrowserProcess {
    #[cfg(unix)]
    pgid: i32,
    #[cfg(unix)]
    _child: tokio::process::Child,
    #[cfg(windows)]
    child: Mutex<Box<dyn process_wrap::tokio::TokioChildWrapper>>,
}

impl BrowserProcess {
    fn spawn(executable: &Path, args: &[String]) -> BrowserResult<Self> {
        let mut cmd = tokio::process::Command::new(executable);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            cmd.kill_on_drop(true).process_group(0);
            let child = cmd.spawn().map_err(|e| {
                BrowserError::LaunchFailed(format!("spawn {}: {e}", executable.display()))
            })?;
            let pgid = child
                .id()
                .map(|p| p as i32)
                .ok_or_else(|| BrowserError::LaunchFailed("browser has no pid".into()))?;
            Ok(Self {
                pgid,
                _child: child,
            })
        }
        #[cfg(windows)]
        {
            // The same Job Object mechanism `CommandRunner` uses: killing the
            // job kills every descendant renderer/GPU process, so a browser
            // tree can never outlive the daemon that started it.
            use process_wrap::tokio::*;
            let mut wrap = TokioCommandWrap::from(cmd);
            wrap.wrap(JobObject);
            wrap.wrap(KillOnDrop);
            let child = wrap.spawn().map_err(|e| {
                BrowserError::LaunchFailed(format!("spawn {}: {e}", executable.display()))
            })?;
            Ok(Self {
                child: Mutex::new(child),
            })
        }
    }

    async fn reap(&self) {
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.pgid, libc::SIGTERM);
            tokio::time::sleep(Duration::from_millis(200)).await;
            libc::killpg(self.pgid, libc::SIGKILL);
        }
        #[cfg(windows)]
        {
            let mut child = self.child.lock().await;
            let _ = Box::into_pin(child.kill()).await;
        }
    }
}

// ── the backend ──────────────────────────────────────────────────────────────

/// One attached page target.
#[derive(Clone)]
struct Attached {
    target_id: String,
    session_id: String,
}

pub struct CdpBackend {
    conn: Arc<Connection>,
    process: BrowserProcess,
    tabs: Mutex<HashMap<TabId, Attached>>,
    next_tab: AtomicU64,
}

impl CdpBackend {
    /// Start `executable` under `profile_dir` and connect to it.
    pub async fn launch(executable: &Path, profile_dir: &Path) -> BrowserResult<Self> {
        std::fs::create_dir_all(profile_dir)
            .map_err(|e| BrowserError::ProfileUnavailable(format!("create profile dir: {e}")))?;
        // A stale port file from a previous run would be read as this run's.
        let port_file = profile_dir.join("DevToolsActivePort");
        let _ = std::fs::remove_file(&port_file);

        let args = launch_args(profile_dir);
        let process = BrowserProcess::spawn(executable, &args)?;
        let ws_url = wait_for_devtools(&port_file, LAUNCH_TIMEOUT).await?;
        let conn = Connection::open(&ws_url).await?;
        // Auto-attach so a `window.open` popup is a target we already hold a
        // session for by the time the model asks about tabs.
        conn.call(
            None,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
            ACTION_TIMEOUT,
        )
        .await?;
        Ok(Self {
            conn,
            process,
            tabs: Mutex::new(HashMap::new()),
            next_tab: AtomicU64::new(1),
        })
    }

    /// Attach to one page target and enable the domains this backend reads.
    async fn attach(&self, target_id: &str) -> BrowserResult<String> {
        let res = self
            .conn
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
                ACTION_TIMEOUT,
            )
            .await?;
        let session_id = res
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::ActionFailed("attach returned no sessionId".into()))?
            .to_string();
        for domain in ["Page", "Runtime", "DOM", "Log", "Network"] {
            self.conn
                .call(
                    Some(&session_id),
                    &format!("{domain}.enable"),
                    json!({}),
                    ACTION_TIMEOUT,
                )
                .await?;
        }
        Ok(session_id)
    }

    async fn session_of(&self, tab: &TabId) -> BrowserResult<String> {
        self.tabs
            .lock()
            .await
            .get(tab)
            .map(|a| a.session_id.clone())
            .ok_or_else(|| BrowserError::TabClosed(format!("unknown tab {tab}")))
    }

    /// Reconcile the tab map with the browser's live page targets: adopt pages
    /// this backend has not seen (popups), drop pages the browser has closed.
    async fn sync_targets(&self) -> BrowserResult<()> {
        let res = self
            .conn
            .call(None, "Target.getTargets", json!({}), ACTION_TIMEOUT)
            .await?;
        let mut live: Vec<(String, String)> = Vec::new(); // (targetId, url)
        if let Some(infos) = res.get("targetInfos").and_then(Value::as_array) {
            for t in infos {
                if t.get("type").and_then(Value::as_str) != Some("page") {
                    continue;
                }
                let url = t.get("url").and_then(Value::as_str).unwrap_or("");
                if url.starts_with("devtools://") || url.starts_with("chrome-extension://") {
                    continue;
                }
                if let Some(id) = t.get("targetId").and_then(Value::as_str) {
                    live.push((id.to_string(), url.to_string()));
                }
            }
        }
        let live_ids: Vec<&str> = live.iter().map(|(id, _)| id.as_str()).collect();
        {
            let mut tabs = self.tabs.lock().await;
            tabs.retain(|_, a| live_ids.contains(&a.target_id.as_str()));
        }
        let known: Vec<String> = self
            .tabs
            .lock()
            .await
            .values()
            .map(|a| a.target_id.clone())
            .collect();
        for (target_id, _) in &live {
            if known.contains(target_id) {
                continue;
            }
            let session_id = self.attach(target_id).await?;
            let tab = TabId::new(format!(
                "tab-{}",
                self.next_tab.fetch_add(1, Ordering::Relaxed)
            ));
            self.tabs.lock().await.insert(
                tab,
                Attached {
                    target_id: target_id.clone(),
                    session_id,
                },
            );
        }
        Ok(())
    }

    async fn eval(&self, session: &str, expression: &str) -> BrowserResult<Value> {
        let res = self
            .conn
            .call(
                Some(session),
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
                ACTION_TIMEOUT,
            )
            .await?;
        if let Some(d) = res.get("exceptionDetails") {
            return Err(BrowserError::ActionFailed(format!(
                "page script failed: {}",
                d.get("text").and_then(Value::as_str).unwrap_or("exception")
            )));
        }
        Ok(res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Resolve a ref to a live remote object, or fail as stale. Never widens
    /// the query — a missing attribute is a missing element (§18).
    async fn object_for(&self, session: &str, r#ref: &str) -> BrowserResult<String> {
        // Two layers, in this order: the ref is escaped for CSS, then the
        // finished selector is emitted as a JSON string literal — which is
        // also a valid JS string literal. Neither layer can be escaped from.
        let selector = format!(
            "[data-leveler-ref={}]",
            crate::backend::css_string_literal(r#ref)
        );
        let expr = format!("document.querySelector({})", js_string_literal(&selector));
        let res = self
            .conn
            .call(
                Some(session),
                "Runtime.evaluate",
                json!({"expression": expr}),
                ACTION_TIMEOUT,
            )
            .await?;
        res.get("result")
            .and_then(|r| r.get("objectId"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                BrowserError::RefStale(format!("{ref} matches no element in the current page"))
            })
    }

    async fn release(&self, session: &str, object_id: &str) {
        let _ = self
            .conn
            .call(
                Some(session),
                "Runtime.releaseObject",
                json!({"objectId": object_id}),
                ACTION_TIMEOUT,
            )
            .await;
    }

    async fn call_on(
        &self,
        session: &str,
        object_id: &str,
        function: &str,
        args: Vec<Value>,
    ) -> BrowserResult<Value> {
        let res = self
            .conn
            .call(
                Some(session),
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": function,
                    "arguments": args.into_iter().map(|v| json!({"value": v})).collect::<Vec<_>>(),
                    "returnByValue": true,
                    "userGesture": true,
                }),
                ACTION_TIMEOUT,
            )
            .await?;
        if let Some(d) = res.get("exceptionDetails") {
            return Err(BrowserError::ActionFailed(format!(
                "element script failed: {}",
                d.get("text").and_then(Value::as_str).unwrap_or("exception")
            )));
        }
        Ok(res
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// The element's viewport centre, after scrolling it into view.
    async fn centre_of(&self, session: &str, object_id: &str) -> BrowserResult<(f64, f64)> {
        self.call_on(
            session,
            object_id,
            "function(){ this.scrollIntoView({block:'center', inline:'center'}); }",
            vec![],
        )
        .await?;
        let model = self
            .conn
            .call(
                Some(session),
                "DOM.getBoxModel",
                json!({"objectId": object_id}),
                ACTION_TIMEOUT,
            )
            .await?;
        let quad = model
            .get("model")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                BrowserError::ActionFailed("element has no layout box to interact with".into())
            })?;
        if quad.len() < 8 {
            return Err(BrowserError::ActionFailed(
                "element has no layout box to interact with".into(),
            ));
        }
        let n = |i: usize| quad[i].as_f64().unwrap_or(0.0);
        Ok(((n(0) + n(4)) / 2.0, (n(1) + n(5)) / 2.0))
    }

    async fn mouse(
        &self,
        session: &str,
        kind: &str,
        x: f64,
        y: f64,
        clicks: u32,
    ) -> BrowserResult<()> {
        self.conn
            .call(
                Some(session),
                "Input.dispatchMouseEvent",
                json!({
                    "type": kind, "x": x, "y": y,
                    "button": if kind == "mouseMoved" { "none" } else { "left" },
                    "buttons": if kind == "mousePressed" { 1 } else { 0 },
                    "clickCount": clicks,
                }),
                ACTION_TIMEOUT,
            )
            .await?;
        Ok(())
    }

    async fn key(&self, session: &str, key: &str) -> BrowserResult<()> {
        let (code, vk, text) = key_descriptor(key);
        for kind in ["keyDown", "keyUp"] {
            let mut params = json!({
                "type": kind,
                "key": key,
                "code": code,
                "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk,
            });
            if kind == "keyDown"
                && let Some(t) = text
            {
                params["text"] = json!(t);
                params["unmodifiedText"] = json!(t);
            }
            self.conn
                .call(
                    Some(session),
                    "Input.dispatchKeyEvent",
                    params,
                    ACTION_TIMEOUT,
                )
                .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl BrowserBackend for CdpBackend {
    async fn new_tab(&self) -> BrowserResult<TabId> {
        let res = self
            .conn
            .call(
                None,
                "Target.createTarget",
                json!({"url": "about:blank"}),
                ACTION_TIMEOUT,
            )
            .await?;
        let target_id = res
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::ActionFailed("createTarget returned no id".into()))?
            .to_string();
        let session_id = self.attach(&target_id).await?;
        let tab = TabId::new(format!(
            "tab-{}",
            self.next_tab.fetch_add(1, Ordering::Relaxed)
        ));
        self.tabs.lock().await.insert(
            tab.clone(),
            Attached {
                target_id,
                session_id,
            },
        );
        Ok(tab)
    }

    async fn close_tab(&self, tab: &TabId) -> BrowserResult<()> {
        let target_id = self
            .tabs
            .lock()
            .await
            .get(tab)
            .map(|a| a.target_id.clone())
            .ok_or_else(|| BrowserError::TabClosed(format!("unknown tab {tab}")))?;
        self.conn
            .call(
                None,
                "Target.closeTarget",
                json!({"targetId": target_id}),
                ACTION_TIMEOUT,
            )
            .await?;
        self.tabs.lock().await.remove(tab);
        Ok(())
    }

    async fn list_tabs(&self) -> BrowserResult<Vec<RawTab>> {
        self.sync_targets().await?;
        let tabs: Vec<(TabId, String)> = self
            .tabs
            .lock()
            .await
            .iter()
            .map(|(t, a)| (t.clone(), a.session_id.clone()))
            .collect();
        let mut out = Vec::new();
        for (tab, session) in tabs {
            let (url, title) = self.locate_session(&session).await.unwrap_or_default();
            out.push(RawTab { tab, url, title });
        }
        out.sort_by(|a, b| a.tab.cmp(&b.tab));
        Ok(out)
    }

    async fn navigate(&self, tab: &TabId, url: &str) -> BrowserResult<()> {
        let session = self.session_of(tab).await?;
        let res = self
            .conn
            .call(
                Some(&session),
                "Page.navigate",
                json!({"url": url}),
                NAV_TIMEOUT,
            )
            .await?;
        if let Some(err) = res.get("errorText").and_then(Value::as_str) {
            return Err(BrowserError::ActionFailed(format!(
                "navigation to {url} failed: {err}"
            )));
        }
        self.await_ready(&session, NAV_TIMEOUT).await
    }

    async fn reload(&self, tab: &TabId) -> BrowserResult<()> {
        let session = self.session_of(tab).await?;
        self.conn
            .call(Some(&session), "Page.reload", json!({}), NAV_TIMEOUT)
            .await?;
        self.await_ready(&session, NAV_TIMEOUT).await
    }

    async fn locate(&self, tab: &TabId) -> BrowserResult<(String, String)> {
        let session = self.session_of(tab).await?;
        self.locate_session(&session).await
    }

    async fn snapshot(&self, tab: &TabId, generation: u64) -> BrowserResult<RawSnapshot> {
        let session = self.session_of(tab).await?;
        let value = self.eval(&session, &snapshot_script(generation)).await?;
        raw_snapshot_from(value)
    }

    async fn act(&self, tab: &TabId, act: &Act<'_>) -> BrowserResult<()> {
        let session = self.session_of(tab).await?;
        match act {
            Act::Click { r#ref } => {
                let obj = self.object_for(&session, r#ref).await?;
                let out = async {
                    let (x, y) = self.centre_of(&session, &obj).await?;
                    self.mouse(&session, "mouseMoved", x, y, 0).await?;
                    self.mouse(&session, "mousePressed", x, y, 1).await?;
                    self.mouse(&session, "mouseReleased", x, y, 1).await
                }
                .await;
                self.release(&session, &obj).await;
                out
            }
            Act::Fill { r#ref, text } | Act::Type { r#ref, text } => {
                let append = matches!(act, Act::Type { .. });
                let obj = self.object_for(&session, r#ref).await?;
                let out = async {
                    self.conn
                        .call(
                            Some(&session),
                            "DOM.focus",
                            json!({"objectId": obj}),
                            ACTION_TIMEOUT,
                        )
                        .await?;
                    // Replace = select the existing value first, so the real
                    // insert overwrites it; append = caret to the end.
                    let f = if append {
                        "function(){ if (this.setSelectionRange && this.value != null) \
                         this.setSelectionRange(this.value.length, this.value.length); }"
                    } else {
                        "function(){ if (this.select) this.select(); \
                         else if (this.isContentEditable) document.getSelection().selectAllChildren(this); }"
                    };
                    self.call_on(&session, &obj, f, vec![]).await?;
                    self.conn
                        .call(
                            Some(&session),
                            "Input.insertText",
                            json!({"text": text}),
                            ACTION_TIMEOUT,
                        )
                        .await?;
                    Ok::<(), BrowserError>(())
                }
                .await;
                self.release(&session, &obj).await;
                out
            }
            Act::Press { r#ref, key } => {
                if let Some(r) = r#ref {
                    let obj = self.object_for(&session, r).await?;
                    let _ = self
                        .conn
                        .call(
                            Some(&session),
                            "DOM.focus",
                            json!({"objectId": obj}),
                            ACTION_TIMEOUT,
                        )
                        .await;
                    self.release(&session, &obj).await;
                }
                self.key(&session, key).await
            }
            Act::Select { r#ref, values } => {
                let obj = self.object_for(&session, r#ref).await?;
                let out = self
                    .call_on(&session, &obj, &select_fn(), vec![json!(values)])
                    .await
                    .and_then(select_outcome);
                self.release(&session, &obj).await;
                out
            }
            Act::Scroll { r#ref, dx, dy } => {
                let (x, y) = match r#ref {
                    Some(r) => {
                        let obj = self.object_for(&session, r).await?;
                        let c = self.centre_of(&session, &obj).await;
                        self.release(&session, &obj).await;
                        c?
                    }
                    None => {
                        let v = self.eval(&session, "[innerWidth/2, innerHeight/2]").await?;
                        let a = v.as_array().cloned().unwrap_or_default();
                        (
                            a.first().and_then(Value::as_f64).unwrap_or(0.0),
                            a.get(1).and_then(Value::as_f64).unwrap_or(0.0),
                        )
                    }
                };
                self.conn
                    .call(
                        Some(&session),
                        "Input.dispatchMouseEvent",
                        json!({"type":"mouseWheel","x":x,"y":y,"deltaX":dx,"deltaY":dy}),
                        ACTION_TIMEOUT,
                    )
                    .await?;
                Ok(())
            }
        }
    }

    async fn screenshot(&self, tab: &TabId) -> BrowserResult<String> {
        let session = self.session_of(tab).await?;
        let res = self
            .conn
            .call(
                Some(&session),
                "Page.captureScreenshot",
                json!({"format": "png"}),
                ACTION_TIMEOUT,
            )
            .await?;
        res.get("data")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| BrowserError::ActionFailed("no screenshot data".into()))
    }

    async fn inspect(&self, tab: &TabId, kind: InspectKind) -> BrowserResult<InspectReport> {
        let session = self.session_of(tab).await?;
        let map = self.conn.observations.lock().await;
        let obs = map.get(&session);
        Ok(match kind {
            InspectKind::Console => {
                InspectReport::Console(obs.map(|o| o.console.clone()).unwrap_or_default())
            }
            InspectKind::PageErrors => {
                InspectReport::Console(obs.map(|o| o.page_errors.clone()).unwrap_or_default())
            }
            InspectKind::Network => {
                InspectReport::Network(obs.map(|o| o.network.clone()).unwrap_or_default())
            }
        })
    }

    async fn is_live(&self) -> bool {
        !self.conn.is_dead()
    }

    async fn shutdown(&self) {
        let _ = self
            .conn
            .call(None, "Browser.close", json!({}), Duration::from_secs(3))
            .await;
        self.process.reap().await;
    }
}

impl CdpBackend {
    async fn locate_session(&self, session: &str) -> BrowserResult<(String, String)> {
        let v = self
            .eval(session, "[location.href, document.title]")
            .await?;
        let a = v.as_array().cloned().unwrap_or_default();
        Ok((
            a.first().and_then(Value::as_str).unwrap_or("").to_string(),
            a.get(1).and_then(Value::as_str).unwrap_or("").to_string(),
        ))
    }

    /// Wait until the document has parsed. Polling, deliberately: a one-shot
    /// wait needs no event subscription and cannot lose a race with one.
    async fn await_ready(&self, session: &str, timeout: Duration) -> BrowserResult<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(v) = self.eval(session, "document.readyState").await
                && matches!(v.as_str(), Some("interactive") | Some("complete"))
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::Timeout {
                    stage: "navigate".into(),
                    message: "the document did not finish parsing".into(),
                });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// The CDP wrapper around the shared `<select>` body.
fn select_fn() -> String {
    format!(
        "function(values){{ var el = this; {} }}",
        crate::backend::SELECT_BODY
    )
}

fn select_outcome(v: Value) -> BrowserResult<()> {
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    Err(BrowserError::ActionFailed(format!(
        "select failed: {}",
        v.get("why").and_then(Value::as_str).unwrap_or("unknown")
    )))
}

fn raw_snapshot_from(value: Value) -> BrowserResult<RawSnapshot> {
    let get = |k: &str| {
        value
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let num = |k: &str| value.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
    if value.is_null() {
        return Err(BrowserError::ActionFailed(
            "the snapshot script returned nothing".into(),
        ));
    }
    Ok(RawSnapshot {
        url: get("url"),
        title: get("title"),
        text: get("text"),
        nodes: num("nodes"),
        total: num("total"),
    })
}

/// The flags CodeLeveler starts a CDP browser with. Deliberately short: a
/// remote-debugging port, an isolated profile, and the first-run interstitials
/// off. Nothing here changes what the page can reach.
fn launch_args(profile_dir: &Path) -> Vec<String> {
    vec![
        "--remote-debugging-port=0".into(),
        format!("--user-data-dir={}", profile_dir.display()),
        "--headless=new".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-search-engine-choice-screen".into(),
        "about:blank".into(),
    ]
}

/// Read the port the browser chose, then build its WebSocket URL. The file is
/// written only once the DevTools endpoint is actually listening.
async fn wait_for_devtools(port_file: &Path, timeout: Duration) -> BrowserResult<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(port_file) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next())
                && !port.trim().is_empty()
            {
                return Ok(format!("ws://127.0.0.1:{}{}", port.trim(), path.trim()));
            }
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::LaunchFailed(format!(
                "the browser did not publish a DevTools endpoint within {}s",
                timeout.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A JS string literal. JSON's string encoding is a subset of JavaScript's,
/// so serialising through `Value::String` produces a literal the page parses
/// identically — with every quote and backslash escaped.
fn js_string_literal(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

/// `code` / virtual key / text for the keys an agent presses.
fn key_descriptor(key: &str) -> (&'static str, u32, Option<&'static str>) {
    match key {
        "Enter" => ("Enter", 13, Some("\r")),
        "Tab" => ("Tab", 9, Some("\t")),
        "Escape" => ("Escape", 27, None),
        "Backspace" => ("Backspace", 8, None),
        "Delete" => ("Delete", 46, None),
        "ArrowUp" => ("ArrowUp", 38, None),
        "ArrowDown" => ("ArrowDown", 40, None),
        "ArrowLeft" => ("ArrowLeft", 37, None),
        "ArrowRight" => ("ArrowRight", 39, None),
        "Home" => ("Home", 36, None),
        "End" => ("End", 35, None),
        "PageUp" => ("PageUp", 33, None),
        "PageDown" => ("PageDown", 34, None),
        " " | "Space" => ("Space", 32, Some(" ")),
        _ => ("", 0, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ref that tries to close the attribute selector and add a second one
    /// must end up inert inside a single string, at BOTH layers.
    #[test]
    fn a_ref_cannot_break_out_of_the_page_expression() {
        for hostile in [
            r#"a"] ,*[x="#,
            r#"a']  ,*[x='"#,
            r#"a\"] ,*[x=\""#,
            r#"'; alert(1); '"#,
        ] {
            let selector = format!(
                "[data-leveler-ref={}]",
                crate::backend::css_string_literal(hostile)
            );
            let expr = format!("document.querySelector({})", js_string_literal(&selector));
            // The whole expression is one call on one string literal: the only
            // unescaped quotes are the two that delimit it.
            let head = "document.querySelector(\"";
            assert!(expr.starts_with(head) && expr.ends_with("\")"), "{expr}");
            let inner = &expr[head.len()..expr.len() - 2];
            let mut chars = inner.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    chars.next();
                } else {
                    assert_ne!(c, '"', "an unescaped quote closes the literal: {expr}");
                }
            }
        }
    }

    #[test]
    fn launch_args_carry_the_profile_and_ask_for_an_ephemeral_port() {
        let args = launch_args(Path::new("/tmp/p"));
        assert!(args.iter().any(|a| a == "--remote-debugging-port=0"));
        assert!(args.iter().any(|a| a == "--user-data-dir=/tmp/p"));
    }

    #[test]
    fn named_keys_carry_a_virtual_key_code() {
        assert_eq!(key_descriptor("Enter").1, 13);
        assert_eq!(key_descriptor("ArrowDown").1, 40);
        assert_eq!(key_descriptor("Escape").2, None);
        assert_eq!(key_descriptor("Enter").2, Some("\r"));
    }

    #[test]
    fn a_protocol_error_about_a_missing_node_is_a_stale_ref() {
        let e = map_protocol_error(&json!({"message":"No node with given id found"}));
        assert!(matches!(e, BrowserError::RefStale(_)), "{e:?}");
        let e = map_protocol_error(&json!({"message":"Target closed"}));
        assert!(matches!(e, BrowserError::TabClosed(_)), "{e:?}");
    }

    #[test]
    fn select_reports_a_miss_instead_of_claiming_success() {
        assert!(select_outcome(json!({"ok": true})).is_ok());
        let e = select_outcome(json!({"ok": false, "why": "no option matched"})).unwrap_err();
        assert!(e.to_string().contains("no option matched"), "{e}");
    }
}
