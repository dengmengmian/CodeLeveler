//! The reducer's input ([`Action`]) and output ([`Effect`]).
//!
//! Terminal input and runtime events are both funneled into `Action`; the
//! reducer folds them into [`AppState`] and returns `Effect`s the event loop
//! performs at the edge (send a command, quit). This keeps the reducer pure and
//! testable without a terminal or a live client .

use crossterm::event::{KeyEvent, MouseEvent};

use leveler_client_protocol::{ClientCommand, CommandId, RuntimeEvent, UiSessionSnapshot};

use crate::state::PendingInteraction;

/// Result produced by an asynchronous edge effect and folded back through the
/// reducer. Keeping completions as actions prevents network and filesystem
/// latency from blocking terminal input or runtime events.
#[derive(Debug, Clone)]
pub enum EffectCompletion {
    CommandDelivered,
    CommandFailed {
        /// Best-effort authoritative state used to roll back optimistic UI.
        snapshot: Option<Box<UiSessionSnapshot>>,
    },
    InteractionDelivered {
        key: String,
    },
    InteractionUncertain {
        key: String,
        restore: PendingInteraction,
        /// Boxed so the enum stays small (snapshot is a large reconnect payload).
        snapshot: Option<Box<UiSessionSnapshot>>,
    },
}

/// Something that happened and needs to be folded into state.
///
/// `Runtime` is the largest variant, but an `Action` is short-lived — one is
/// created per event and consumed by `reduce` immediately, never stored in
/// bulk — so boxing it would only add noise at every construction site.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum Action {
    /// `/remote` finished, one way or another.
    Remote(RemoteOutcome),
    /// An event from the runtime.
    Runtime(RuntimeEvent),
    /// A key press.
    Key(KeyEvent),
    /// Mouse wheel / drag / click (Conversation viewport).
    Mouse(MouseEvent),
    /// Drive edge auto-scroll while a text selection drag is active.
    SelectionTick,
    /// A burst of plain text typed into the composer.
    TextInput(String),
    /// A bracketed-paste payload.
    Paste(String),
    /// The terminal was resized to (cols, rows).
    Resize(u16, u16),
    /// Project file paths loaded at the terminal edge for `@file` completion.
    FileCandidatesLoaded(Vec<String>),
    /// An asynchronous edge effect completed.
    EffectCompleted(EffectCompletion),
    /// The embedded Web UI server finished starting: `Ok(url)` with the
    /// token-carrying URL, or `Err(message)` if it could not start.
    WebLaunched(Result<String, String>),
}

/// A side effect for the event loop to carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Send a command to the runtime.
    Send(ClientCommand),
    /// Send an approval/clarification answer with a stable [`CommandId`] for
    /// at-least-once retries. On transport failure the event loop confirms
    /// delivery via snapshot before restoring `restore`.
    SendInteraction {
        command: ClientCommand,
        restore: crate::state::PendingInteraction,
        /// Idempotency key — reused across retries of the same decision.
        command_id: CommandId,
    },
    /// Load repository files without blocking the pure reducer.
    LoadFileCandidates { repository: String },
    /// Start the embedded browser Web UI server (`/web`). The event loop runs
    /// the injected [`WebLauncher`] and folds its result back as
    /// [`Action::WebLaunched`].
    StartWeb,
    /// Open a URL in the default browser (conversation link click, or `/web`
    /// re-invocation when the server is already up).
    OpenWebUrl(String),
    /// Make this machine reachable from a paired phone and produce an invite
    /// (`/remote`). `local` binds the relay to this machine's LAN address
    /// instead of expecting one on the internet — a phone on the same Wi-Fi can
    /// reach that, and it needs no server anywhere.
    StartRemote { local: bool },
    /// Accept or reject the device waiting to pair, from the invite screen.
    AnswerPairing { accept: bool },
    /// Tear down the UI and exit.
    Quit,
}

/// What `/remote` produced: everything the invite screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteInvite {
    /// The QR, already rendered as terminal rows. The TUI does not encode it
    /// itself — that would put a QR library in a crate whose job is drawing.
    pub qr: Vec<String>,
    /// The payload the QR encodes, for a phone that cannot scan.
    pub payload: String,
    /// This machine's key fingerprint, for the user to compare on the phone.
    pub host_fingerprint: String,
    /// Where the phone will connect, shown so a user can see it is their own
    /// network rather than someone else's server.
    pub relay_url: String,
}

/// A device waiting for this machine to accept it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRequest {
    pub device_name: String,
    pub platform: String,
    /// What the user compares with the phone's screen. The whole security of
    /// pairing is this one comparison.
    pub fingerprint: String,
}

/// Injected at startup by the CLI: makes this machine reachable and answers the
/// pairing it produces. Opaque so `leveler-tui` needs neither the relay nor the
/// agent — the same reason [`WebLauncher`] is a closure.
pub type RemoteLauncher = std::sync::Arc<
    dyn Fn(
            RemoteRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RemoteOutcome> + Send>>
        + Send
        + Sync,
>;

/// What the TUI is asking the host side to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteRequest {
    /// Start (if needed) and produce an invite.
    Invite {
        local: bool,
    },
    /// Is anybody waiting to be accepted?
    Pending,
    Accept,
    Reject,
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteOutcome {
    Invited(RemoteInvite),
    Waiting(Option<PairingRequest>),
    Paired { device_name: String },
    Rejected,
    Failed(String),
}

/// Injected at startup by the CLI: binds and serves the browser Web UI over the
/// current in-process runtime, returning the token-carrying URL (or an error
/// message). Kept as an opaque closure so `leveler-tui` need not depend on the
/// web server or the local-transport service trait. `None` when the runtime
/// cannot back a Web UI (e.g. a TUI attached to a remote daemon).
pub type WebLauncher = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;
