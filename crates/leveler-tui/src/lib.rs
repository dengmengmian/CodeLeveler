//! `leveler-tui` — the CodeLeveler interactive terminal UI .
//!
//! A Ratatui/Crossterm/Tokio terminal shell built on a single-direction data
//! flow: terminal + runtime events become [`Action`]s, the pure [`reduce`]
//! folds them into [`AppState`], and [`render`] draws it. The UI talks to the
//! runtime **only** through [`leveler_client_protocol`] — never provider, tool,
//! or storage internals .
//!
//! [`Action`]: action::Action
//! [`reduce`]: reducer::reduce
//! [`AppState`]: state::AppState
//! [`render`]: render::render
#![forbid(unsafe_code)]

pub mod action;
mod activity_stream;
mod code_block;
pub mod composer;
mod diff_view;
mod footer_queue;
pub mod i18n;
pub mod inline;
pub mod markdown;
pub mod overlay;
mod plan_cell;
pub mod reducer;
pub mod render;
pub mod screen;
mod selection;
mod splash;
pub mod state;
mod status_line;
pub mod terminal;
pub mod theme;
mod theme_config;
mod tool_cell;
mod tool_result;
pub mod tool_taxonomy;
pub mod transcript;
mod workbench;

mod run;

pub use action::WebLauncher;
pub use i18n::Locale;
pub use run::{TuiError, open_in_browser, run};
pub use state::Boot;
pub use theme::{Theme, ThemeId};
pub use theme_config::{load_theme_id, save_theme_id};
