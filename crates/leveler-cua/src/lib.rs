//! Native desktop observation and control as one capability.
//!
//! The crate owns the platform backends, the observation generation and the
//! element references. It does not own an agent loop, a session, an approval
//! decision or a tool surface: those stay with the runtime and `leveler-tools`.
//!
//! One process-lived worker thread serializes every platform call. Serializing
//! GUI work is a correctness property, not a performance shortcut: two actions
//! racing on the same window cannot both claim the observation they acted on.
//!
//! Observation state is per session. Two sessions may drive the desktop at the
//! same time; one session's tree observation must not invalidate another's
//! references. The caller passes a stable session scope on every call.

mod backend;
mod error;
mod state;
mod types;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

mod platform {
    #[cfg(target_os = "macos")]
    pub(crate) use crate::macos as imp;
    #[cfg(not(any(target_os = "macos", windows)))]
    pub(crate) use crate::unsupported as imp;
    #[cfg(windows)]
    pub(crate) use crate::windows as imp;
}

#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported {
    use crate::backend::{Backend, Unavailable};

    pub(crate) const NAME: &str = "unsupported";

    pub(crate) fn init_thread() {}

    pub(crate) fn shutdown_thread() {}

    pub(crate) fn new_backend() -> Box<dyn Backend> {
        Box::new(Unavailable::new(format!(
            "native desktop control is not implemented on {}",
            std::env::consts::OS
        )))
    }
}

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use error::{CuaError, CuaResult, PermissionKind};
use state::ObservationState;
pub use types::{
    ActOutcome, ActRequest, ActStatus, AppInfo, Availability, Bounds, ElementInfo, Modifier,
    MouseButton, Observation, PermissionState, Point, Screenshot, WindowInfo,
};
use types::{ActionKind, Expectation, Node, ResolvedAction};

const APPS_TIMEOUT: Duration = Duration::from_secs(10);
const TREE_TIMEOUT: Duration = Duration::from_secs(30);
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);
const ACT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ComputerUse {
    inner: Mutex<Inner>,
}

struct Inner {
    tx: Option<Sender<Request>>,
    sessions: HashMap<String, ObservationState>,
}

struct Request {
    kind: RequestKind,
    cancel: Arc<AtomicBool>,
    reply: SyncSender<CuaResult<Response>>,
}

enum RequestKind {
    Availability,
    Apps,
    Windows {
        app: Option<String>,
    },
    Tree {
        window: u64,
        depth: u32,
        max_nodes: u32,
    },
    Screenshot {
        window: Option<u64>,
    },
    Act(ResolvedAction),
}

enum Response {
    Availability(Availability),
    Apps(Vec<AppInfo>),
    Windows(Vec<WindowInfo>),
    Tree {
        window: WindowInfo,
        root: Node,
        truncated: bool,
    },
    Screenshot(Screenshot),
    Act(ActOutcome),
}

impl Default for ComputerUse {
    fn default() -> Self {
        Self::new()
    }
}

impl ComputerUse {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                tx: None,
                sessions: HashMap::new(),
            }),
        }
    }

    /// Whether this build has a native backend at all. It says nothing about
    /// the current TCC grant or the presence of a graphical session; those are
    /// call-time facts reported by [`Self::availability`].
    pub const fn platform_supported() -> bool {
        cfg!(any(target_os = "macos", windows))
    }

    pub fn availability(&self) -> CuaResult<Availability> {
        let mut inner = self.lock();
        match inner.call(RequestKind::Availability, APPS_TIMEOUT, None)? {
            Response::Availability(value) => Ok(value),
            _ => Err(unexpected("availability")),
        }
    }

    pub fn apps(&self) -> CuaResult<Vec<AppInfo>> {
        let mut inner = self.lock();
        match inner.call(RequestKind::Apps, APPS_TIMEOUT, None)? {
            Response::Apps(value) => Ok(value),
            _ => Err(unexpected("apps")),
        }
    }

    pub fn windows(&self, scope: &str, app: Option<&str>) -> CuaResult<Vec<WindowInfo>> {
        let mut inner = self.lock();
        let windows = match inner.call(
            RequestKind::Windows {
                app: app.map(str::to_string),
            },
            APPS_TIMEOUT,
            None,
        )? {
            Response::Windows(value) => value,
            _ => return Err(unexpected("windows")),
        };
        inner.state(scope).record_windows(&windows);
        Ok(windows)
    }

    /// Read one window's accessibility tree. The returned references are valid
    /// until the same session's next tree observation, and every action
    /// re-validates them against the live UI.
    pub fn observe_window(
        &self,
        scope: &str,
        window: u64,
        depth: u32,
        max_nodes: u32,
        cancel: Option<Arc<AtomicBool>>,
    ) -> CuaResult<Observation> {
        let mut inner = self.lock();
        let (window, root, truncated) = match inner.call(
            RequestKind::Tree {
                window,
                depth: depth.clamp(1, 8),
                max_nodes: max_nodes.clamp(20, 800),
            },
            TREE_TIMEOUT,
            cancel,
        )? {
            Response::Tree {
                window,
                root,
                truncated,
            } => (window, root, truncated),
            _ => return Err(unexpected("tree")),
        };
        let node_count = root.node_count();
        let state = inner.state(scope);
        let generation = state.begin_tree();
        let root = state.register_tree(generation, &window, &root);
        Ok(Observation {
            window,
            root,
            node_count,
            truncated,
        })
    }

    pub fn screenshot(&self, window: Option<u64>) -> CuaResult<Screenshot> {
        let mut inner = self.lock();
        match inner.call(RequestKind::Screenshot { window }, SCREENSHOT_TIMEOUT, None)? {
            Response::Screenshot(value) => Ok(value),
            _ => Err(unexpected("screenshot")),
        }
    }

    pub fn act(&self, scope: &str, request: ActRequest, activate: bool) -> CuaResult<ActOutcome> {
        let mut inner = self.lock();
        let action = resolve_action(inner.state(scope), request, activate)?;
        match inner.call(RequestKind::Act(action), ACT_TIMEOUT, None)? {
            Response::Act(value) => Ok(value),
            _ => Err(unexpected("act")),
        }
    }

    /// A short label for an approval prompt, from the last observation only.
    /// It never performs I/O and never waits for an in-flight desktop call, so
    /// it is safe to call while admission is deciding.
    pub fn target_label(
        &self,
        scope: &str,
        window: Option<u64>,
        reference: Option<&str>,
    ) -> Option<String> {
        let inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        let state = inner.sessions.get(scope)?;
        if let Some(reference) = reference
            && let Ok(record) = state.resolve(reference)
        {
            let name = record
                .title
                .as_deref()
                .or(record.identifier.as_deref())
                .unwrap_or(&record.role);
            return Some(format!("{} {name:?}", record.role));
        }
        window.and_then(|window| {
            state
                .observed_window(window)
                .map(|observed| format!("{} {:?}", observed.app, observed.title))
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Inner {
    fn state(&mut self, scope: &str) -> &mut ObservationState {
        self.sessions.entry(scope.to_string()).or_default()
    }

    fn call(
        &mut self,
        kind: RequestKind,
        timeout: Duration,
        cancel: Option<Arc<AtomicBool>>,
    ) -> CuaResult<Response> {
        self.ensure_worker();
        let (reply, receiver) = sync_channel(1);
        let request = Request {
            kind,
            cancel: cancel.unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
            reply,
        };
        let Some(tx) = self.tx.as_ref() else {
            return Err(CuaError::Backend {
                backend: platform::imp::NAME,
                detail: "worker is unavailable".into(),
            });
        };
        if tx.send(request).is_err() {
            self.tx = None;
            return Err(CuaError::Backend {
                backend: platform::imp::NAME,
                detail: "worker stopped before the request was accepted".into(),
            });
        }
        match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(CuaError::Timeout(format!(
                "the {} backend did not answer within {}s; the request may still have been carried out",
                platform::imp::NAME,
                timeout.as_secs()
            ))),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.tx = None;
                Err(CuaError::Backend {
                    backend: platform::imp::NAME,
                    detail: "worker stopped while the request was running; the effect is unknown"
                        .into(),
                })
            }
        }
    }

    fn ensure_worker(&mut self) {
        if self.tx.is_some() {
            return;
        }
        let (tx, rx) = channel::<Request>();
        let spawned = std::thread::Builder::new()
            .name(format!("leveler-cua-{}", platform::imp::NAME))
            .spawn(move || worker_loop(rx));
        match spawned {
            Ok(_) => self.tx = Some(tx),
            Err(error) => {
                tracing::error!(%error, "could not start the desktop backend worker");
            }
        }
    }
}

fn worker_loop(rx: Receiver<Request>) {
    platform::imp::init_thread();
    let mut backend = platform::imp::new_backend();
    while let Ok(request) = rx.recv() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch(&mut *backend, &request)
        }))
        .unwrap_or_else(|_| {
            Err(CuaError::Backend {
                backend: platform::imp::NAME,
                detail: "the backend panicked; the effect is unknown".into(),
            })
        });
        let _ = request.reply.send(result);
    }
    platform::imp::shutdown_thread();
}

fn dispatch(backend: &mut dyn backend::Backend, request: &Request) -> CuaResult<Response> {
    match &request.kind {
        RequestKind::Availability => backend.availability().map(Response::Availability),
        RequestKind::Apps => backend.apps().map(Response::Apps),
        RequestKind::Windows { app } => backend.windows(app.as_deref()).map(Response::Windows),
        RequestKind::Tree {
            window,
            depth,
            max_nodes,
        } => backend
            .tree(*window, *depth, *max_nodes, &request.cancel)
            .map(|(window, root, truncated)| Response::Tree {
                window,
                root,
                truncated,
            }),
        RequestKind::Screenshot { window } => backend.screenshot(*window).map(Response::Screenshot),
        RequestKind::Act(action) => backend.act(action).map(Response::Act),
    }
}

fn unexpected(operation: &str) -> CuaError {
    CuaError::Backend {
        backend: platform::imp::NAME,
        detail: format!("worker answered the wrong value for {operation}"),
    }
}

fn resolve_action(
    state: &ObservationState,
    request: ActRequest,
    activate: bool,
) -> CuaResult<ResolvedAction> {
    let reference_record = |reference: &str| state.resolve(reference);
    let require_observed = |window: u64| -> CuaResult<Bounds> {
        state
            .observed_window(window)
            .map(|observed| observed.bounds)
            .ok_or_else(|| {
                CuaError::InvalidAction(format!(
                    "observe window {window} before directing input at it; coordinates and focus are only meaningful against an observation"
                ))
            })
    };
    match request {
        ActRequest::ActivateApp { pid } => {
            if pid == 0 {
                return Err(CuaError::InvalidAction("activate_app needs a pid".into()));
            }
            Ok(ResolvedAction {
                action: ActionKind::ActivateApp { pid },
                expectation: None,
                activate: true,
            })
        }
        ActRequest::FocusWindow { window } => {
            require_observed(window)?;
            Ok(ResolvedAction {
                action: ActionKind::FocusWindow { window },
                expectation: None,
                activate: true,
            })
        }
        ActRequest::Click {
            window,
            reference,
            point,
            button,
            count,
        } => {
            let count = count.clamp(1, 3);
            match (reference, point) {
                (Some(reference), None) => {
                    let record = reference_record(&reference)?;
                    check_window(window, record.window)?;
                    Ok(ResolvedAction {
                        action: ActionKind::Click {
                            window: record.window,
                            path: Some(record.path.clone()),
                            point: None,
                            guard_bounds: None,
                            button,
                            count,
                        },
                        expectation: Some(expectation(&record)),
                        activate,
                    })
                }
                (None, Some(point)) => {
                    let window = window.ok_or_else(|| {
                        CuaError::InvalidAction(
                            "a coordinate click needs the window it was observed in".into(),
                        )
                    })?;
                    let guard_bounds = require_observed(window)?;
                    Ok(ResolvedAction {
                        action: ActionKind::Click {
                            window,
                            path: None,
                            point: Some(point),
                            guard_bounds: Some(guard_bounds),
                            button,
                            count,
                        },
                        expectation: None,
                        activate,
                    })
                }
                (Some(_), Some(_)) => Err(CuaError::InvalidAction(
                    "pass either an element reference or a screen point, not both".into(),
                )),
                (None, None) => Err(CuaError::InvalidAction(
                    "click needs an element reference or an x/y screen point".into(),
                )),
            }
        }
        ActRequest::Type {
            window,
            reference,
            text,
            submit,
        } => {
            if text.is_empty() {
                return Err(CuaError::InvalidAction("type needs non-empty text".into()));
            }
            match reference {
                Some(reference) => {
                    let record = reference_record(&reference)?;
                    check_window(window, record.window)?;
                    Ok(ResolvedAction {
                        action: ActionKind::Type {
                            window: record.window,
                            path: Some(record.path.clone()),
                            text,
                            submit,
                        },
                        expectation: Some(expectation(&record)),
                        activate,
                    })
                }
                None => {
                    let window = window.ok_or_else(|| {
                        CuaError::InvalidAction(
                            "keyboard input needs the window it was observed in".into(),
                        )
                    })?;
                    require_observed(window)?;
                    Ok(ResolvedAction {
                        action: ActionKind::Type {
                            window,
                            path: None,
                            text,
                            submit,
                        },
                        expectation: None,
                        activate,
                    })
                }
            }
        }
        ActRequest::Key {
            window,
            key,
            modifiers,
        } => {
            if key.trim().is_empty() {
                return Err(CuaError::InvalidAction("key needs a key name".into()));
            }
            require_observed(window)?;
            Ok(ResolvedAction {
                action: ActionKind::Key {
                    window,
                    key,
                    modifiers,
                },
                expectation: None,
                activate,
            })
        }
        ActRequest::Scroll {
            window,
            point,
            guard_bounds,
            dx,
            dy,
        } => {
            let observed = require_observed(window)?;
            let guard_bounds = match (guard_bounds, point) {
                (Some(bounds), _) => Some(bounds),
                (None, Some(_)) => Some(observed),
                (None, None) => None,
            };
            Ok(ResolvedAction {
                action: ActionKind::Scroll {
                    window,
                    point,
                    guard_bounds,
                    dx,
                    dy,
                },
                expectation: None,
                activate,
            })
        }
        ActRequest::SetValue {
            window,
            reference,
            value,
        } => {
            let record = reference_record(&reference)?;
            check_window(window, record.window)?;
            Ok(ResolvedAction {
                action: ActionKind::SetValue {
                    window: record.window,
                    path: record.path.clone(),
                    value,
                },
                expectation: Some(expectation(&record)),
                activate,
            })
        }
        ActRequest::PerformAction {
            window,
            reference,
            action,
        } => {
            if action.trim().is_empty() {
                return Err(CuaError::InvalidAction(
                    "perform_action needs an accessibility action name".into(),
                ));
            }
            let record = reference_record(&reference)?;
            check_window(window, record.window)?;
            Ok(ResolvedAction {
                action: ActionKind::PerformAction {
                    window: record.window,
                    path: record.path.clone(),
                    action,
                },
                expectation: Some(expectation(&record)),
                activate,
            })
        }
    }
}

fn check_window(given: Option<u64>, resolved: u64) -> CuaResult<()> {
    match given {
        Some(window) if window != resolved => Err(CuaError::InvalidAction(format!(
            "the reference belongs to window {resolved}, not {window}"
        ))),
        _ => Ok(()),
    }
}

fn expectation(record: &state::RefRecord) -> Expectation {
    Expectation {
        role: record.role.clone(),
        identifier: record.identifier.clone(),
        title: record.title.clone(),
        path: record.path.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Bounds, MouseButton, Point, WindowInfo};

    fn observed_state() -> ObservationState {
        let mut state = ObservationState::default();
        state.record_windows(&[WindowInfo {
            id: 9,
            pid: 77,
            app: "Fixture".into(),
            title: "Fixture Window".into(),
            bounds: Bounds {
                x: 100.0,
                y: 100.0,
                width: 400.0,
                height: 300.0,
            },
            on_screen: true,
            focused: true,
        }]);
        state
    }

    #[test]
    fn a_coordinate_click_must_name_a_window_that_was_observed() {
        let state = ObservationState::default();
        let error = resolve_action(
            &state,
            ActRequest::Click {
                window: Some(9),
                reference: None,
                point: Some(Point { x: 1.0, y: 2.0 }),
                button: MouseButton::Left,
                count: 1,
            },
            false,
        )
        .unwrap_err();
        assert!(matches!(error, CuaError::InvalidAction(_)));

        let action = resolve_action(
            &observed_state(),
            ActRequest::Click {
                window: Some(9),
                reference: None,
                point: Some(Point { x: 1.0, y: 2.0 }),
                button: MouseButton::Left,
                count: 1,
            },
            false,
        )
        .expect("observed window");
        match action.action {
            ActionKind::Click {
                guard_bounds: Some(bounds),
                ..
            } => assert_eq!(bounds.width, 400.0),
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn a_reference_and_a_point_are_mutually_exclusive() {
        let error = resolve_action(
            &observed_state(),
            ActRequest::Click {
                window: Some(9),
                reference: Some("e0-0".into()),
                point: Some(Point { x: 1.0, y: 2.0 }),
                button: MouseButton::Left,
                count: 1,
            },
            false,
        )
        .unwrap_err();
        assert!(matches!(error, CuaError::InvalidAction(_)));
    }
}
