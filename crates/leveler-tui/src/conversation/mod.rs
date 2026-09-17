//! Conversation subsystem: the ONE owner of viewport state and geometry.
//!
//! Everything about "where the conversation is on screen and which content
//! line a screen cell means" lives here. The renderer paints with this
//! geometry, the reducer maps mouse/scroll intents through it — neither side
//! re-derives layout facts on its own. That single-ownership rule exists
//! because the worst interaction bug this UI ever shipped ("click A, group B
//! expands") was exactly two modules computing the same scroll two ways.

pub mod build;
pub mod geometry;
pub mod interaction;
pub mod view;
pub mod viewport;

pub use view::{ConvCacheEntry, ConvKey, ConversationView};

use crate::state::AppState;

/// Freeze the viewport at the position the renderer actually painted BEFORE
/// leaving auto-follow. While auto-following, the painted scroll is computed
/// at paint time and `conv.scroll` in state is stale; flipping the flag first
/// and mapping a mouse event against the stale value would hit rows the user
/// is not looking at. Order matters: capture the live scroll, then disable
/// auto-follow, then hit-test.
pub fn pin_at_current_viewport(state: &mut AppState) {
    if state.conv.auto_scroll {
        let width = geometry::content_width(state);
        let height = geometry::viewport_height(state);
        let total = crate::conversation::build::conversation_line_count(state, width);
        state.conv.scroll = geometry::max_scroll(total, height);
    }
    state.conv.auto_scroll = false;
}

/// Clamp scroll after resize / content change; track unread growth while the
/// user reads history. Returns true if state changed (a repaint is due).
///
/// Called from the run loop after layout-affecting updates so a user who
/// scrolled up is not shoved past the content end, and auto-follow sticks to
/// the latest activity line. Dimensions come from the authoritative geometry —
/// never passed in by the caller.
pub fn sync_scroll(state: &mut AppState) -> bool {
    let width = geometry::content_width(state);
    let height = geometry::viewport_height(state);
    let total = crate::conversation::build::conversation_line_count(state, width);
    let max_scroll = geometry::max_scroll(total, height);
    let mut changed = false;

    // Track growth while the user is reading history → drive ▼ N.
    if !state.conv.auto_scroll && total > state.conv.last_len {
        state.conv.unread = state.conv.unread.saturating_add(1);
        changed = true;
    }
    if state.conv.auto_scroll {
        state.conv.unread = 0;
    }
    if state.conv.last_len != total {
        state.conv.last_len = total;
        changed = true;
    }

    if state.conv.auto_scroll {
        if state.conv.scroll != max_scroll {
            state.conv.scroll = max_scroll;
            changed = true;
        }
        return changed;
    }
    if state.conv.scroll > max_scroll {
        state.conv.scroll = max_scroll;
        changed = true;
    }
    changed
}
