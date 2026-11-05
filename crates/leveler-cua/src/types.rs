//! Values exchanged with a desktop backend.
//!
//! Coordinates are logical screen points with the origin at the top-left of
//! the primary display, matching what Accessibility and CoreGraphics report.
//! Screenshot pixels are often a multiple of those points (`scale`), so a
//! model that reasons about the image must divide by the scale before pointing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Bounds {
    pub fn center(&self) -> Point {
        Point {
            x: self.x + self.width / 2.0,
            y: self.y + self.height / 2.0,
        }
    }

    pub fn approximately_equals(&self, other: &Self) -> bool {
        (self.x - other.x).abs() <= 2.0
            && (self.y - other.y).abs() <= 2.0
            && (self.width - other.width).abs() <= 2.0
            && (self.height - other.height).abs() <= 2.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppInfo {
    pub pid: u32,
    pub name: String,
    pub window_count: usize,
    pub frontmost: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    /// Platform window identity: `CGWindowID` on macOS, `HWND` as u64 on
    /// Windows. It is an observation handle, not durable state.
    pub id: u64,
    pub pid: u32,
    pub app: String,
    pub title: String,
    pub bounds: Bounds,
    pub on_screen: bool,
    pub focused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementInfo {
    /// Opaque reference valid until the next tree observation of this session.
    pub reference: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subrole: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focused: Option<bool>,
    pub actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<ElementInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub window: WindowInfo,
    pub root: ElementInfo,
    pub node_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct Screenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    /// Human-readable target, e.g. `window 10746 "codelevler"` or `main display`.
    pub target: String,
    pub window: Option<WindowInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Granted,
    Denied,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Availability {
    pub platform: String,
    pub supported: bool,
    pub interactive_session: bool,
    pub accessibility: PermissionState,
    pub screen_recording: PermissionState,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Command,
    Control,
    Alt,
    Shift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActStatus {
    /// The platform action succeeded and a re-observation saw the expected
    /// change.
    Applied,
    /// The platform action succeeded but no observable change could be
    /// verified. The call happened; the effect is not proven.
    CalledOnly,
    /// The platform action succeeded and the observed state is unchanged.
    Unchanged,
    /// The platform action was issued but the result could not be observed.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActOutcome {
    pub action: String,
    pub target: String,
    pub status: ActStatus,
    pub detail: String,
    pub window: Option<WindowInfo>,
    pub frontmost_app: Option<AppInfo>,
}

/// A model-requested action before reference resolution.
///
/// A reference is preferred for any element. A window alone is enough for
/// keyboard and pointer input, but only after that window has been observed;
/// the facade refuses to aim at a window the model has not seen.
#[derive(Debug, Clone)]
pub enum ActRequest {
    ActivateApp {
        pid: u32,
    },
    FocusWindow {
        window: u64,
    },
    Click {
        window: Option<u64>,
        reference: Option<String>,
        point: Option<Point>,
        button: MouseButton,
        count: u8,
    },
    Type {
        window: Option<u64>,
        reference: Option<String>,
        text: String,
        submit: bool,
    },
    Key {
        window: u64,
        key: String,
        modifiers: Vec<Modifier>,
    },
    Scroll {
        window: u64,
        point: Option<Point>,
        guard_bounds: Option<Bounds>,
        dx: f64,
        dy: f64,
    },
    SetValue {
        window: Option<u64>,
        reference: String,
        value: String,
    },
    PerformAction {
        window: Option<u64>,
        reference: String,
        action: String,
    },
}

/// A validated desktop action. The facade resolves references into paths and
/// expectations before this reaches a backend; the backend re-reads the live
/// window and element and refuses a mismatch.
// The platform backends are the only consumers. On other targets the facade
// still validates and rejects the action as unsupported, so the value is
// constructed but intentionally never read.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct ResolvedAction {
    pub action: ActionKind,
    pub expectation: Option<Expectation>,
    pub activate: bool,
}

#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) enum ActionKind {
    ActivateApp {
        pid: u32,
    },
    FocusWindow {
        window: u64,
    },
    Click {
        window: u64,
        path: Option<Vec<usize>>,
        point: Option<Point>,
        guard_bounds: Option<Bounds>,
        button: MouseButton,
        count: u8,
    },
    Type {
        window: u64,
        path: Option<Vec<usize>>,
        text: String,
        submit: bool,
    },
    Key {
        window: u64,
        key: String,
        modifiers: Vec<Modifier>,
    },
    Scroll {
        window: u64,
        point: Option<Point>,
        guard_bounds: Option<Bounds>,
        dx: f64,
        dy: f64,
    },
    SetValue {
        window: u64,
        path: Vec<usize>,
        value: String,
    },
    PerformAction {
        window: u64,
        path: Vec<usize>,
        action: String,
    },
}

/// The element identity captured at observation time. A backend re-reads these
/// attributes before acting; a mismatch means the UI changed under the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expectation {
    pub role: String,
    pub identifier: Option<String>,
    pub title: Option<String>,
    pub path: Vec<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub path: Vec<usize>,
    pub role: String,
    pub subrole: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub identifier: Option<String>,
    pub enabled: Option<bool>,
    pub focused: Option<bool>,
    pub actions: Vec<String>,
    pub bounds: Option<Bounds>,
    pub children: Vec<Node>,
}

impl Node {
    pub(crate) fn node_count(&self) -> usize {
        1 + self.children.iter().map(Node::node_count).sum::<usize>()
    }
}
