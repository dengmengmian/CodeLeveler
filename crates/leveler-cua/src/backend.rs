//! The platform boundary.
//!
//! A backend performs mechanical observation and input. It does not decide
//! whether an action is allowed, whether a reference is current, or what the
//! model should do next; those are owned by the tool and the facade.

use std::sync::atomic::AtomicBool;

use crate::error::CuaResult;
use crate::types::{
    ActOutcome, AppInfo, Availability, Node, ResolvedAction, Screenshot, WindowInfo,
};

pub(crate) trait Backend: Send {
    fn availability(&mut self) -> CuaResult<Availability>;
    fn apps(&mut self) -> CuaResult<Vec<AppInfo>>;
    fn windows(&mut self, app: Option<&str>) -> CuaResult<Vec<WindowInfo>>;
    fn tree(
        &mut self,
        window: u64,
        depth: u32,
        max_nodes: u32,
        cancel: &AtomicBool,
    ) -> CuaResult<(WindowInfo, Node, bool)>;
    fn screenshot(&mut self, window: Option<u64>) -> CuaResult<Screenshot>;
    fn act(&mut self, action: &ResolvedAction) -> CuaResult<ActOutcome>;
}

/// A backend that can only answer why it is not there. Linux uses it today.
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) struct Unavailable {
    detail: String,
}

#[cfg(not(any(target_os = "macos", windows)))]
impl Unavailable {
    pub(crate) fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    fn error(&self) -> crate::error::CuaError {
        crate::error::CuaError::Unsupported(self.detail.clone())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
impl Backend for Unavailable {
    fn availability(&mut self) -> CuaResult<Availability> {
        Ok(Availability {
            platform: std::env::consts::OS.to_string(),
            supported: false,
            interactive_session: false,
            accessibility: crate::types::PermissionState::Unsupported,
            screen_recording: crate::types::PermissionState::Unsupported,
            notes: vec![self.detail.clone()],
        })
    }
    fn apps(&mut self) -> CuaResult<Vec<AppInfo>> {
        Err(self.error())
    }
    fn windows(&mut self, _app: Option<&str>) -> CuaResult<Vec<WindowInfo>> {
        Err(self.error())
    }
    fn tree(
        &mut self,
        _window: u64,
        _depth: u32,
        _max_nodes: u32,
        _cancel: &AtomicBool,
    ) -> CuaResult<(WindowInfo, Node, bool)> {
        Err(self.error())
    }
    fn screenshot(&mut self, _window: Option<u64>) -> CuaResult<Screenshot> {
        Err(self.error())
    }
    fn act(&mut self, _action: &ResolvedAction) -> CuaResult<ActOutcome> {
        Err(self.error())
    }
}
