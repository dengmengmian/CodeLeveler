//! Presentation layer: reusable visual vocabulary, independent of any one
//! domain. A component here receives a small presentation model built by a
//! domain adapter (Agent tool groups today; user-shell executions or
//! capability runs tomorrow) and paints it — it never inspects tool names,
//! transcript items, or runtime types.

pub mod disclosure;
