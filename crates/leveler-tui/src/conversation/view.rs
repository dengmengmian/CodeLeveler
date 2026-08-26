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
}

/// One memoized conversation build: cache key, wrapped lines, and the
/// disclosure hit rows (absolute line index → transcript item index). The hit
/// rows are rebuilt with the lines under the same key, so they can never go
/// stale relative to what is painted.
pub type ConvCacheEntry = (
    ConvKey,
    std::rc::Rc<Vec<Line<'static>>>,
    std::rc::Rc<Vec<(usize, usize)>>,
);

/// Viewport + interaction state for the Conversation.
#[derive(Debug)]
pub struct ConversationView {
    /// Scroll offset (in content lines) from the top.
    pub scroll: usize,
    /// When true, stick to the bottom as new activity arrives.
    pub auto_scroll: bool,
    /// Content ticks observed while pinned away from bottom (for ▼ N).
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
        }
    }
}
