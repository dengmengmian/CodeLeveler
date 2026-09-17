//! Workspace read, search and edit capability.
//!
//! `read`, `ls`, `find`, `grep`, `edit` and `write` are model-facing adapters
//! over the three implementations here. Keeping the capability in one module —
//! not one crate per capability — is deliberate: a responsibility boundary
//! does not need a compilation boundary (`docs/ARCHITECTURE.md` §5.3).

pub(crate) mod editor;
pub(crate) mod reader;
pub(crate) mod search;

pub(crate) use editor::{Commit, WorkspaceEditor};
pub(crate) use reader::{Clip, ReadError, ReadWindow, WorkspaceReader};
pub(crate) use search::{DirEntry, GrepQuery, SearchError, WorkspaceSearch};
