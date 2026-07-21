//! `leveler-lsp` — a Language Server Protocol client (spec §26 phase-2 / §4
//! "complete LSP platform"): launch a language server over stdio, run the
//! initialize handshake, and query document/workspace symbols, definitions, and
//! diagnostics. Vendor-neutral (any LSP server); a registry maps languages to
//! their default servers.
#![forbid(unsafe_code)]

pub mod client;
pub mod codec;
pub mod registry;

pub use client::{Diagnostic, LspClient, LspError, SymbolInfo, SymbolLocation, SymbolSpan};
pub use codec::{FrameReader, encode};
pub use registry::{ServerSpec, server_available, server_available_with_environment, server_for};
