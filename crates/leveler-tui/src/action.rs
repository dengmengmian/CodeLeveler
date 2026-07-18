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
    /// Tear down the UI and exit.
    Quit,
}
