//! Conversation view state: everything the viewport and its interactions own.
//!
//! This is PRESENTATION state — none of it enters the canonical transcript,
//! the event log, or resume/replay. Domain state (transcript items, plan,
//! runtime status) stays on `AppState`; this struct holds only "how the user
//! is currently looking at / touching the conversation".

use ratatui::text::Line;

/// Everything `build_conversation_lines` reads that can change its output.
/// When unchanged, the previously wrapped lines are reused verbatim. The
/// transcript is captured by its monotonic `version`, so any in-place item
/// edit invalidates the cache.
#[derive(Debug, PartialEq, Clone)]
pub struct ConvKey {
    pub(crate) version: u64,
    pub(crate) width: usize,
    pub(crate) theme_id: crate::theme::ThemeId,
    pub(crate) monochrome: bool,
    pub(crate) locale: crate::i18n::Locale,
    pub(crate) tools_expanded: bool,
    /// The call an open approval is holding. Part of the key because opening or
    /// answering an approval changes a transcript ROW (§11), and a cache that
    /// did not notice would keep painting `◌` over a command nobody has
    /// authorised — or `等待批准` after it was allowed.
    pub(crate) awaiting_approval: Option<leveler_client_protocol::ToolCallId>,
    /// The app's turn clock. Running rows (a command's elapsed, a sub-agent's
    /// run time) are painted from it, so a new second is a new frame even when
    /// no transcript event arrived. Constant while idle.
    pub(crate) elapsed_secs: u64,
    /// The diff preview budget derived from the conversation viewport height.
    /// Part of the key because a height-only resize (same width) changes it and
    /// would otherwise keep painting the previous budget's truncation.
    pub(crate) diff_preview_rows: usize,
    /// The command row holding the keyboard focus, when the Command workbench
    /// focus is active. Part of the key because focus paints the row's opener
    /// (§11), and a cache that did not notice would keep two rows marked.
    pub(crate) focused_command: Option<leveler_client_protocol::ToolCallId>,
}

/// One memoized conversation build: cache key, wrapped lines, the disclosure
/// hit rows (absolute line index → transcript item index), the command rows,
/// and the absolute line where the last Final answer begins. Everything is
/// rebuilt under the same key, so no derived index can go stale relative to
/// what is painted.
pub type ConvCacheEntry = (
    ConvKey,
    std::rc::Rc<Vec<Line<'static>>>,
    std::rc::Rc<Vec<(usize, usize)>>,
    std::rc::Rc<Vec<CommandHit>>,
    Option<usize>,
);

/// A command call's clickable row in the built conversation: absolute line,
/// transcript item, call index within its group, and whether the call is
/// stoppable right now (running and not already stopped by this client).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandHit {
    pub line: usize,
    pub item: usize,
    pub call: usize,
    pub stoppable: bool,
}

/// Render inputs shared by every cacheable transcript unit. A unit is
/// re-wrapped only when one of these changes or when the item itself changes.
/// Live inputs (turn clock, approval, focus) are deliberately absent: a
/// cacheable item never reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitEnv {
    pub width: usize,
    pub theme_id: crate::theme::ThemeId,
    pub monochrome: bool,
    pub locale: crate::i18n::Locale,
    pub tools_expanded: bool,
}

/// A command row in unit-relative coordinates (resolved against the assembled
/// conversation when the unit is placed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachedCommand {
    pub line: usize,
    pub item_offset: usize,
    pub call: usize,
    pub stoppable: bool,
}

/// Wrapped lines for one immutable transcript item, plus its disclosure hit
/// rows and command rows in item-relative coordinates. `items` is kept so the
/// next build can tell whether the unit still matches what it wrapped.
#[derive(Debug, Clone)]
pub struct CachedUnit {
    pub items: Vec<crate::transcript::TranscriptItem>,
    pub env: UnitEnv,
    pub lines: std::rc::Rc<Vec<Line<'static>>>,
    pub hits: Vec<(usize, usize)>,
    pub commands: Vec<CachedCommand>,
}

/// Per-item memo of wrapped transcript lines. Positional: entry N is the Nth
/// cacheable unit of the last build. A unit is reused only when its items and
/// env compare equal, so an insertion (compaction) or an in-place edit simply
/// misses rather than reusing the wrong lines.
#[derive(Debug, Default)]
pub struct ItemLineCache {
    pub units: Vec<CachedUnit>,
}

/// Viewport + interaction state for the Conversation.
#[derive(Debug)]
pub struct ConversationView {
    /// Scroll offset (in content lines) from the top.
    pub scroll: usize,
    /// When true, stick to the bottom as new activity arrives.
    pub auto_scroll: bool,
    /// Content ticks observed while pinned away from bottom (for the
    /// scroll-to-bottom badge, which counts content LINES).
    pub unread: usize,
    /// Last seen conversation line count (to detect growth while scrolled up).
    pub last_len: usize,
    /// Last painted Conversation rect (x, y, w, h) — the authoritative
    /// viewport for every geometry computation.
    pub rect: Option<(u16, u16, u16, u16)>,
    /// Last painted scroll-to-bottom button rect, if visible.
    pub scroll_bottom_rect: Option<(u16, u16, u16, u16)>,
    /// Text selection (mouse drag copy).
    pub selection: crate::selection::TextSelection,
    /// Edge auto-scroll while dragging a selection: `-1` up, `0` none, `1` down.
    pub selection_edge_dir: i8,
    /// Consecutive edge-scroll ticks (accelerates step size).
    pub selection_edge_streak: u32,
    /// Last mouse cell while dragging (screen col/row), for remapping after scroll.
    pub selection_last_mouse: Option<(u16, u16)>,
    /// Cached plain-text of conversation lines for the last render width
    /// (backs selection extraction and URL hit-testing).
    pub plain: Vec<String>,
    /// Content width used when `plain` was built.
    pub plain_width: usize,
    /// Memoized wrapped conversation lines + disclosure hit rows. Interior
    /// mutability so read-only render/measure paths can populate it.
    pub cache: std::cell::RefCell<Option<ConvCacheEntry>>,
    /// Memoized wrapped lines per immutable transcript item. This is what keeps
    /// a long finalized history from being re-wrapped on every streaming frame:
    /// only the item whose content changed is recomputed.
    pub item_cache: std::cell::RefCell<ItemLineCache>,
}

impl Default for ConversationView {
    fn default() -> Self {
        Self {
            scroll: 0,
            auto_scroll: true,
            unread: 0,
            last_len: 0,
            rect: None,
            scroll_bottom_rect: None,
            selection: crate::selection::TextSelection::default(),
            selection_edge_dir: 0,
            selection_edge_streak: 0,
            selection_last_mouse: None,
            plain: Vec::new(),
            plain_width: 0,
            cache: std::cell::RefCell::new(None),
            item_cache: std::cell::RefCell::new(ItemLineCache::default()),
        }
    }
}
