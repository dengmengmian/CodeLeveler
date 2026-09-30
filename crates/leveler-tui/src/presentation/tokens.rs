//! Shared single-glyph presentation tokens.
//!
//! These are the markers that appear on many surfaces. Keeping them here means
//! "the current row" looks the same in a picker, a popup and a clarification
//! instead of each surface spelling it differently.

/// The one marker for **the current focus/selection row** on overlay surfaces:
/// picker cursors, popup selection, approval choices and clarification options
/// all use it.
///
/// Deliberately neither `›` (the composer/list cursor, which stays distinct so
/// an open overlay cannot be confused with the composer prompt) nor `▸` (the
/// folded half of the disclosure pair `▸`/`▾`, see [`super::disclosure`], which
/// means "click to expand" — a different fact about a row than "the cursor is
/// here").
pub const FOCUS_CURSOR: &str = "❯";
