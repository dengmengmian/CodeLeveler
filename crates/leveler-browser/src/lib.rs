//! Browser automation as a CodeLeveler capability.
//!
//! ```text
//! Browser Pack enabled
//!         ↓
//! browser_tab · browser_act · browser_inspect
//!         ↓
//! Browser session          (refs, generations, tab ownership)
//!         ↓
//! Chrome / Edge / Chromium over CDP     or     Safari over WebDriver
//! ```
//!
//! Three facts define this crate:
//!
//! - **The user's default browser is the one that gets driven.** A product is
//!   chosen by call, then `[browser].default`, then the operating system's
//!   default. A richer protocol never promotes a product, and an unavailable
//!   one is an error rather than a different browser ([`discover`]).
//! - **A product is not a protocol.** Chrome, Edge and Chromium are three
//!   browsers that happen to share CDP; an Edge default launches Edge.
//! - **The browser is network-authorised.** Exposing the capability IS the
//!   authorisation, so localhost, LAN dev servers and the public internet are
//!   all reachable, and nothing in this crate re-litigates that per request.
//!
//! What CodeLeveler owns: product selection, capability availability, session
//! and tab ownership, refs and their generations, cancellation, and the
//! browser's process lifetime. What the protocols own: navigation, the DOM,
//! rendering, input and observation.

mod backend;
mod cdp;
mod discover;
mod error;
mod session;
mod types;
mod webdriver;

pub use backend::Act;
pub use discover::{Launcher, availability, select_product, system_default_product, which};
pub use error::{BrowserError, BrowserResult};
pub use session::Browser;
pub use types::{
    ActionOutcome, BrowserProduct, BrowserSessionId, BrowserSnapshot, BrowserStatus, ConsoleEntry,
    InspectKind, InspectReport, NetworkEntry, ProductSource, SelectedProduct, TabId, TabInfo,
};
