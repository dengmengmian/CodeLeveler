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
//! - **Browser automation has its own default.** A product is chosen by call,
//!   then `[browser].default`, then the first installed CDP product in the
//!   stable Chrome, Edge, Chromium order. Safari WebDriver is opt-in because
//!   its isolated Automation Window and missing observation channels make it
//!   a poor default for frontend debugging. An explicit or configured product
//!   is never substituted after failure ([`discover`]).
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
pub use discover::{Launcher, automation_default_product, availability, select_product, which};
pub use error::{BrowserError, BrowserResult};
pub use session::Browser;
pub use types::{
    ActionOutcome, BrowserProduct, BrowserSessionId, BrowserSnapshot, BrowserStatus, ConsoleEntry,
    InspectKind, InspectReport, NetworkEntry, ProductSource, SelectedProduct, TabId, TabInfo,
};
