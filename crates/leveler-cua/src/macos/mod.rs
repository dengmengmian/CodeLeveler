//! macOS backend: Accessibility (AXUIElement) for semantics and trees,
//! CoreGraphics for window enumeration and input, `screencapture` for pixels.
//!
//! The backend never claims that an action "completed". It reports what the
//! platform accepted and what a fresh observation saw.

mod capture;
mod cf;
mod input;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::backend::Backend;
use crate::error::{CuaError, CuaResult, PermissionKind};
use crate::types::{
    ActOutcome, ActStatus, ActionKind, AppInfo, Availability, Bounds, Expectation, Modifier,
    MouseButton, Node, PermissionState, Point, ResolvedAction, Screenshot, WindowInfo,
};

use cf::{
    AX_VALUE_CG_POINT, AX_VALUE_CG_SIZE, AXIsProcessTrusted, AXUIElementCopyActionNames,
    AXUIElementCopyAttributeValue, AXUIElementCreateApplication, AXUIElementPerformAction,
    AXUIElementSetAttributeValue, AXUIElementSetMessagingTimeout, AXValueGetType, AXValueGetValue,
    CFTypeRef, CGPointRaw, CGSessionCopyCurrentDictionary, CGSizeRaw, CGWindowListCopyWindowInfo,
    CfOwned, CfString, array_count, array_value, dictionary_value,
};

pub(crate) const NAME: &str = "macos";

const WINDOW_LIST_ALL: u32 = 16;
const WINDOW_LIST_ON_SCREEN: u32 = 1 | 16;
const AX_MESSAGING_TIMEOUT_SECONDS: f32 = 4.0;
const FOCUS_WAIT: Duration = Duration::from_millis(1500);
const FOCUS_POLL: Duration = Duration::from_millis(50);

pub(crate) fn init_thread() {}

pub(crate) fn shutdown_thread() {}

pub(crate) fn new_backend() -> Box<dyn Backend> {
    Box::new(MacBackend)
}

struct MacBackend;

impl Backend for MacBackend {
    fn availability(&mut self) -> CuaResult<Availability> {
        let mut notes = Vec::new();
        let interactive = match interactive_session() {
            Ok(()) => true,
            Err(CuaError::NoInteractiveSession(reason)) => {
                notes.push(reason);
                false
            }
            Err(error) => return Err(error),
        };
        let accessibility = if unsafe { AXIsProcessTrusted() } != 0 {
            PermissionState::Granted
        } else {
            notes.push(
                "Accessibility is not granted; grant it to the process that launched CodeLeveler \
                 in System Settings → Privacy & Security → Accessibility"
                    .into(),
            );
            PermissionState::Denied
        };
        let screen_recording = if unsafe { cf::CGPreflightScreenCaptureAccess() } != 0 {
            PermissionState::Granted
        } else {
            notes.push(
                "Screen Recording is not granted; grant it to the process that launched \
                 CodeLeveler in System Settings → Privacy & Security → Screen & System Audio Recording"
                    .into(),
            );
            PermissionState::Denied
        };
        Ok(Availability {
            platform: NAME.to_string(),
            supported: true,
            interactive_session: interactive,
            accessibility,
            screen_recording,
            notes,
        })
    }

    fn apps(&mut self) -> CuaResult<Vec<AppInfo>> {
        let windows = cg_windows(true)?;
        let frontmost = frontmost_pid(&windows);
        let mut apps: Vec<AppInfo> = Vec::new();
        for window in windows {
            if let Some(existing) = apps.iter_mut().find(|app| app.pid == window.pid) {
                existing.window_count += 1;
                continue;
            }
            apps.push(AppInfo {
                pid: window.pid,
                name: window.app.clone(),
                window_count: 1,
                frontmost: Some(window.pid) == frontmost,
            });
        }
        Ok(apps)
    }

    fn windows(&mut self, app: Option<&str>) -> CuaResult<Vec<WindowInfo>> {
        let windows = cg_windows(true)?;
        let frontmost = frontmost_pid(&windows);
        let focused = focus_for(frontmost);
        let app = app.map(str::trim).filter(|value| !value.is_empty());
        let mut result = Vec::new();
        for window in windows {
            if !app_matches(app, &window) {
                continue;
            }
            result.push(WindowInfo {
                id: window.id,
                pid: window.pid,
                app: window.app.clone(),
                title: window.title.clone(),
                bounds: window.bounds,
                on_screen: window.on_screen,
                focused: focused == Some(window.id),
            });
        }
        Ok(result)
    }

    fn tree(
        &mut self,
        window: u64,
        depth: u32,
        max_nodes: u32,
        cancel: &AtomicBool,
    ) -> CuaResult<(WindowInfo, Node, bool)> {
        require_interactive()?;
        require_accessibility()?;
        let target = cg_window(window)?;
        let app = app_element(target.pid)?;
        let ax_windows = ax_windows(&app)?;
        let ax_window = choose_ax_window(&ax_windows, &target)
            .ok_or_else(|| CuaError::WindowNotFound(window))?;
        let mut walk = Walk {
            remaining: max_nodes as usize,
            truncated: false,
            cancel,
            depth,
        };
        let root = walk.node(ax_window.element.as_ptr(), Vec::new(), 0)?;
        let focused = focused_window_id(&target.pid);
        Ok((
            WindowInfo {
                id: target.id,
                pid: target.pid,
                app: target.app.clone(),
                title: target.title.clone(),
                bounds: target.bounds,
                on_screen: target.on_screen,
                focused: focused == Some(target.id),
            },
            root,
            walk.truncated,
        ))
    }

    fn screenshot(&mut self, window: Option<u64>) -> CuaResult<Screenshot> {
        require_interactive()?;
        let info = match window {
            Some(window) => Some(window_info(window)?),
            None => None,
        };
        capture::capture(info.as_ref())
    }

    fn act(&mut self, action: &ResolvedAction) -> CuaResult<ActOutcome> {
        require_interactive()?;
        require_accessibility()?;
        match &action.action {
            ActionKind::ActivateApp { pid } => activate_app(*pid),
            ActionKind::FocusWindow { window } => focus_window(*window),
            ActionKind::Click {
                window,
                path,
                point,
                guard_bounds,
                button,
                count,
            } => click(
                *window,
                path.as_deref(),
                *point,
                *guard_bounds,
                *button,
                *count,
                action,
            ),
            ActionKind::Type {
                window,
                path,
                text,
                submit,
            } => type_text(
                *window,
                path.as_deref(),
                text,
                *submit,
                action.expectation.as_ref(),
                action.activate,
            ),
            ActionKind::Key {
                window,
                key,
                modifiers,
            } => post_key(*window, key, modifiers, action.activate),
            ActionKind::Scroll {
                window,
                point,
                guard_bounds,
                dx,
                dy,
            } => scroll(*window, *point, *guard_bounds, *dx, *dy, action.activate),
            ActionKind::SetValue {
                window,
                path,
                value,
            } => set_value(*window, path, value, action.expectation.as_ref()),
            ActionKind::PerformAction {
                window,
                path,
                action: name,
            } => perform_action(*window, path, name, action.expectation.as_ref()),
        }
    }
}

// ── session / permission ────────────────────────────────────────────────────

fn require_interactive() -> CuaResult<()> {
    interactive_session()
}

fn interactive_session() -> CuaResult<()> {
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    let dictionary = unsafe { CfOwned::from_create(raw) }.ok_or_else(|| {
        CuaError::NoInteractiveSession("no graphical login session is attached".into())
    })?;
    let on_console = lookup_bool(
        &dictionary,
        &["kCGSSessionOnConsoleKey", "CGSSessionOnConsoleKey"],
    )
    .unwrap_or(true);
    let locked = lookup_bool(
        &dictionary,
        &["CGSSessionScreenIsLocked", "kCGSSessionScreenIsLocked"],
    )
    .unwrap_or(false);
    if locked {
        return Err(CuaError::NoInteractiveSession(
            "the screen is locked; desktop input cannot reach the user's session".into(),
        ));
    }
    if !on_console {
        return Err(CuaError::NoInteractiveSession(
            "the user is not on the console session".into(),
        ));
    }
    Ok(())
}

fn lookup_bool(dictionary: &CfOwned, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| dictionary_value(dictionary, key).and_then(|value| value.as_bool()))
}

fn require_accessibility() -> CuaResult<()> {
    if unsafe { AXIsProcessTrusted() } != 0 {
        Ok(())
    } else {
        Err(CuaError::PermissionDenied {
            kind: PermissionKind::Accessibility,
            detail: "Accessibility is not granted to the process that launched CodeLeveler; \
                     System Settings → Privacy & Security → Accessibility"
                .into(),
        })
    }
}

// ── CoreGraphics window list ────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct CgWindow {
    id: u64,
    pid: u32,
    app: String,
    title: String,
    bounds: Bounds,
    on_screen: bool,
    alpha: f64,
}

fn cg_windows(all: bool) -> CuaResult<Vec<CgWindow>> {
    let option = if all {
        WINDOW_LIST_ALL
    } else {
        WINDOW_LIST_ON_SCREEN
    };
    let raw = unsafe { CGWindowListCopyWindowInfo(option, 0) };
    let array = unsafe { CfOwned::from_create(raw) }
        .ok_or_else(|| backend("CGWindowListCopyWindowInfo returned null"))?;
    let count = array_count(&array);
    let mut windows = Vec::new();
    for index in 0..count {
        let value = array_value(&array, index);
        let Some(dictionary) = CfOwned::retain(value) else {
            continue;
        };
        let layer = dictionary_value(&dictionary, "kCGWindowLayer")
            .and_then(|value| value.as_i64())
            .unwrap_or(0);
        if layer != 0 {
            continue;
        }
        let Some(id) = dictionary_value(&dictionary, "kCGWindowNumber").and_then(|v| v.as_i64())
        else {
            continue;
        };
        let Some(pid) = dictionary_value(&dictionary, "kCGWindowOwnerPID").and_then(|v| v.as_i64())
        else {
            continue;
        };
        let app = dictionary_value(&dictionary, "kCGWindowOwnerName")
            .and_then(|value| value.as_string())
            .unwrap_or_default();
        let title = dictionary_value(&dictionary, "kCGWindowName")
            .and_then(|value| value.as_string())
            .unwrap_or_default();
        let Some(bounds) = dictionary_value(&dictionary, "kCGWindowBounds")
            .and_then(|value| bounds_from_dictionary(&value))
        else {
            continue;
        };
        let alpha = dictionary_value(&dictionary, "kCGWindowAlpha")
            .and_then(|value| value.as_double())
            .unwrap_or(1.0);
        let on_screen = dictionary_value(&dictionary, "kCGWindowIsOnscreen")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if bounds.width <= 1.0 || bounds.height <= 1.0 || alpha <= 0.01 {
            continue;
        }
        windows.push(CgWindow {
            id: id.max(0) as u64,
            pid: pid.max(0) as u32,
            app,
            title,
            bounds,
            on_screen,
            alpha,
        });
    }
    Ok(windows)
}

fn bounds_from_dictionary(dictionary: &CfOwned) -> Option<Bounds> {
    let read = |key: &str| {
        dictionary_value(dictionary, key)
            .and_then(|value| value.as_double())
            .map(|value| value)
    };
    Some(Bounds {
        x: read("X")?,
        y: read("Y")?,
        width: read("Width")?,
        height: read("Height")?,
    })
}

fn frontmost_pid(windows: &[CgWindow]) -> Option<u32> {
    windows
        .iter()
        .find(|window| window.on_screen && window.alpha > 0.01)
        .map(|window| window.pid)
}

/// The window server id of the AX window the application reports as focused.
///
/// `AXFocusedWindow` on the application is the authoritative attribute: some
/// applications return success for `AXFocused` on a window but keep reporting
/// it as false, so per-window flags alone miss a real focus change.
fn focused_window_id(pid: &u32) -> Option<u64> {
    let all = cg_windows(true).ok()?;
    let app = app_element(*pid).ok()?;
    let (title, bounds) = match ax_attr(app.as_ptr(), "AXFocusedWindow") {
        Some(element) => (
            ax_string(element.as_ptr(), "AXTitle"),
            ax_bounds(element.as_ptr()),
        ),
        None => {
            let windows = ax_windows(&app).ok()?;
            let focused = windows.iter().find(|window| window.focused)?;
            (focused.title.clone(), focused.bounds)
        }
    };
    let bounds = bounds?;
    let cg = all.iter().find(|candidate| {
        candidate.pid == *pid
            && bounds.approximately_equals(&candidate.bounds)
            && title
                .as_deref()
                .is_none_or(|title| title.is_empty() || title == candidate.title)
    })?;
    Some(cg.id)
}

fn focus_for(frontmost: Option<u32>) -> Option<u64> {
    let pid = frontmost?;
    focused_window_id(&pid)
}

fn window_bounds_match(window: &AxWindow, target: &CgWindow) -> bool {
    window
        .bounds
        .is_some_and(|bounds| bounds.approximately_equals(&target.bounds))
}

fn cg_window(id: u64) -> CuaResult<CgWindow> {
    cg_windows(true)?
        .into_iter()
        .find(|window| window.id == id)
        .ok_or(CuaError::WindowNotFound(id))
}

fn window_info(id: u64) -> CuaResult<WindowInfo> {
    let target = cg_window(id)?;
    let focused = focused_window_id(&target.pid);
    Ok(WindowInfo {
        id: target.id,
        pid: target.pid,
        app: target.app.clone(),
        title: target.title.clone(),
        bounds: target.bounds,
        on_screen: target.on_screen,
        focused: focused == Some(target.id),
    })
}

fn app_info(pid: u32) -> Option<AppInfo> {
    let windows = cg_windows(true).ok()?;
    let frontmost = frontmost_pid(&windows);
    let window = windows.iter().find(|window| window.pid == pid)?;
    Some(AppInfo {
        pid,
        name: window.app.clone(),
        window_count: windows.iter().filter(|window| window.pid == pid).count(),
        frontmost: Some(pid) == frontmost,
    })
}

fn app_matches(filter: Option<&str>, window: &CgWindow) -> bool {
    match filter {
        None => true,
        Some(filter) => filter
            .parse::<u32>()
            .map(|pid| pid == window.pid)
            .unwrap_or_else(|_| {
                window
                    .app
                    .to_ascii_lowercase()
                    .contains(&filter.to_ascii_lowercase())
            }),
    }
}

// ── Accessibility ───────────────────────────────────────────────────────────

fn ax_attr(element: cf::AXUIElementRef, name: &str) -> Option<CfOwned> {
    let attribute = CfString::new(name)?;
    let mut value: CFTypeRef = std::ptr::null();
    let code = unsafe { AXUIElementCopyAttributeValue(element, attribute.as_ptr(), &mut value) };
    if code != 0 {
        return None;
    }
    unsafe { CfOwned::from_create(value) }
}

fn ax_string(element: cf::AXUIElementRef, name: &str) -> Option<String> {
    ax_attr(element, name).and_then(|value| value.as_string())
}

fn ax_bool(element: cf::AXUIElementRef, name: &str) -> Option<bool> {
    ax_attr(element, name).and_then(|value| value.as_bool())
}

fn ax_actions(element: cf::AXUIElementRef) -> Vec<String> {
    let mut names = std::ptr::null();
    let code = unsafe { AXUIElementCopyActionNames(element, &mut names) };
    if code != 0 || names.is_null() {
        return Vec::new();
    }
    let Some(array) = (unsafe { CfOwned::from_create(names.cast()) }) else {
        return Vec::new();
    };
    let count = array_count(&array);
    (0..count)
        .filter_map(|index| {
            CfOwned::retain(array_value(&array, index)).and_then(|value| value.as_string())
        })
        .collect()
}

fn ax_bounds(element: cf::AXUIElementRef) -> Option<Bounds> {
    let position = ax_attr(element, "AXPosition")?;
    let size = ax_attr(element, "AXSize")?;
    if unsafe { AXValueGetType(position.as_ptr()) } != AX_VALUE_CG_POINT
        || unsafe { AXValueGetType(size.as_ptr()) } != AX_VALUE_CG_SIZE
    {
        return None;
    }
    let mut point = CGPointRaw::default();
    let mut extent = CGSizeRaw::default();
    let ok_point = unsafe {
        AXValueGetValue(
            position.as_ptr(),
            AX_VALUE_CG_POINT,
            &mut point as *mut CGPointRaw as *mut std::ffi::c_void,
        )
    };
    let ok_size = unsafe {
        AXValueGetValue(
            size.as_ptr(),
            AX_VALUE_CG_SIZE,
            &mut extent as *mut CGSizeRaw as *mut std::ffi::c_void,
        )
    };
    (ok_point != 0 && ok_size != 0).then_some(Bounds {
        x: point.x,
        y: point.y,
        width: extent.width,
        height: extent.height,
    })
}

fn ax_value_string(element: cf::AXUIElementRef) -> Option<String> {
    let value = ax_attr(element, "AXValue")?;
    if value.is_string() {
        return value.as_string();
    }
    if value.is_number() {
        return value.as_double().map(|number| {
            if number.fract() == 0.0 {
                format!("{number:.0}")
            } else {
                number.to_string()
            }
        });
    }
    if value.is_boolean() {
        return value.as_bool().map(|value| value.to_string());
    }
    None
}

fn app_element(pid: u32) -> CuaResult<CfOwned> {
    let raw = unsafe { AXUIElementCreateApplication(pid as i32) };
    let element = unsafe { CfOwned::from_create(raw) }.ok_or_else(|| {
        backend(format!(
            "could not create an accessibility element for pid {pid}"
        ))
    })?;
    unsafe { AXUIElementSetMessagingTimeout(element.as_ptr(), AX_MESSAGING_TIMEOUT_SECONDS) };
    Ok(element)
}

struct AxWindow {
    element: CfOwned,
    title: Option<String>,
    bounds: Option<Bounds>,
    focused: bool,
}

fn ax_windows(app: &CfOwned) -> CuaResult<Vec<AxWindow>> {
    let Some(array) = ax_attr(app.as_ptr(), "AXWindows") else {
        return Err(CuaError::ElementChanged(
            "the application exposes no accessibility windows".into(),
        ));
    };
    let count = array_count(&array);
    let mut windows = Vec::new();
    for index in 0..count {
        let Some(element) = CfOwned::retain(array_value(&array, index)) else {
            continue;
        };
        windows.push(AxWindow {
            title: ax_string(element.as_ptr(), "AXTitle"),
            bounds: ax_bounds(element.as_ptr()),
            focused: ax_bool(element.as_ptr(), "AXFocused").unwrap_or(false),
            element,
        });
    }
    Ok(windows)
}

fn choose_ax_window<'a>(windows: &'a [AxWindow], target: &CgWindow) -> Option<&'a AxWindow> {
    if let Some(exact) = windows.iter().find(|window| {
        window
            .title
            .as_deref()
            .is_some_and(|title| !target.title.is_empty() && title == target.title)
            && window_bounds_match(window, target)
    }) {
        return Some(exact);
    }
    if let Some(by_bounds) = windows
        .iter()
        .find(|window| window_bounds_match(window, target))
    {
        return Some(by_bounds);
    }
    if windows.len() == 1 {
        return windows.first();
    }
    windows.iter().find(|window| {
        window
            .title
            .as_deref()
            .is_some_and(|title| !target.title.is_empty() && title == target.title)
    })
}

struct Walk<'a> {
    remaining: usize,
    truncated: bool,
    cancel: &'a AtomicBool,
    depth: u32,
}

impl Walk<'_> {
    fn node(
        &mut self,
        element: cf::AXUIElementRef,
        path: Vec<usize>,
        level: u32,
    ) -> CuaResult<Node> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(CuaError::Cancelled);
        }
        if self.remaining == 0 {
            self.truncated = true;
        }
        self.remaining = self.remaining.saturating_sub(1);
        let mut node = Node {
            path: path.clone(),
            role: ax_string(element, "AXRole").unwrap_or_default(),
            subrole: ax_string(element, "AXSubrole"),
            title: truncate(ax_string(element, "AXTitle")),
            description: truncate(ax_string(element, "AXDescription")),
            value: truncate(ax_value_string(element)),
            identifier: truncate(ax_string(element, "AXIdentifier")),
            enabled: ax_bool(element, "AXEnabled"),
            focused: ax_bool(element, "AXFocused"),
            actions: ax_actions(element),
            bounds: ax_bounds(element),
            children: Vec::new(),
        };
        if level >= self.depth {
            return Ok(node);
        }
        let Some(children) = ax_attr(element, "AXChildren") else {
            return Ok(node);
        };
        let count = array_count(&children);
        for index in 0..count {
            if self.remaining == 0 {
                self.truncated = true;
                break;
            }
            if self.cancel.load(Ordering::Relaxed) {
                return Err(CuaError::Cancelled);
            }
            let Some(child) = CfOwned::retain(array_value(&children, index)) else {
                continue;
            };
            let mut child_path = path.clone();
            child_path.push(index);
            node.children
                .push(self.node(child.as_ptr(), child_path, level + 1)?);
        }
        Ok(node)
    }
}

fn truncate(value: Option<String>) -> Option<String> {
    value.map(|value| {
        if value.chars().count() > 300 {
            let mut truncated: String = value.chars().take(300).collect();
            truncated.push('…');
            truncated
        } else {
            value
        }
    })
}

fn resolve_element(
    window: &AxWindow,
    path: &[usize],
    expectation: Option<&Expectation>,
) -> CuaResult<CfOwned> {
    let mut element = CfOwned::retain(window.element.as_ptr())
        .ok_or_else(|| CuaError::ElementChanged("the window element is no longer valid".into()))?;
    for index in path {
        let children = ax_attr(element.as_ptr(), "AXChildren").ok_or_else(|| {
            CuaError::ElementChanged(format!(
                "accessibility path {path:?} no longer exists at child {index}"
            ))
        })?;
        if *index >= array_count(&children) {
            return Err(CuaError::ElementChanged(format!(
                "accessibility path {path:?} no longer exists at child {index}"
            )));
        }
        let Some(child) = CfOwned::retain(array_value(&children, *index)) else {
            return Err(CuaError::ElementChanged(format!(
                "accessibility path {path:?} no longer exists at child {index}"
            )));
        };
        element = child;
    }
    if let Some(expectation) = expectation {
        let role = ax_string(element.as_ptr(), "AXRole").unwrap_or_default();
        if role != expectation.role {
            return Err(CuaError::ElementChanged(format!(
                "expected {}, found {role} at {path:?}",
                expectation.role
            )));
        }
        if let Some(identifier) = &expectation.identifier {
            let live = ax_string(element.as_ptr(), "AXIdentifier");
            if live.as_deref() != Some(identifier.as_str()) {
                return Err(CuaError::ElementChanged(format!(
                    "identifier changed from {identifier:?} to {live:?}"
                )));
            }
        }
        if let Some(title) = &expectation.title {
            let live = ax_string(element.as_ptr(), "AXTitle");
            if live.as_deref() != Some(title.as_str()) {
                return Err(CuaError::ElementChanged(format!(
                    "title changed from {title:?} to {live:?}"
                )));
            }
        }
    }
    Ok(element)
}

fn ax_check(code: i32, operation: &str) -> CuaResult<()> {
    match code {
        0 => Ok(()),
        -25211 => Err(CuaError::PermissionDenied {
            kind: PermissionKind::Accessibility,
            detail: format!("{operation}: accessibility API disabled"),
        }),
        -25202 => Err(CuaError::ElementChanged(format!(
            "{operation}: the element no longer exists"
        ))),
        -25204 => Err(backend(format!(
            "{operation}: the application did not respond"
        ))),
        -25206 => Err(CuaError::Unsupported(format!(
            "{operation}: the element does not support this action"
        ))),
        code => Err(backend(format!("{operation}: accessibility error {code}"))),
    }
}

fn set_ax_value(element: &CfOwned, value: &str) -> CuaResult<()> {
    let Some(string) = CfString::new(value) else {
        return Err(CuaError::InvalidAction(
            "the value contains an interior NUL byte".into(),
        ));
    };
    let Some(attribute) = CfString::new("AXValue") else {
        return Err(backend("could not create the AXValue attribute name"));
    };
    let code = unsafe {
        AXUIElementSetAttributeValue(element.as_ptr(), attribute.as_ptr(), string.as_value())
    };
    ax_check(code, "set AXValue")
}

fn set_ax_focused(element: &CfOwned) -> CuaResult<()> {
    let Some(attribute) = CfString::new("AXFocused") else {
        return Err(backend("could not create the AXFocused attribute name"));
    };
    let code = unsafe {
        AXUIElementSetAttributeValue(
            element.as_ptr(),
            attribute.as_ptr(),
            cf::kCFBooleanTrue as CFTypeRef,
        )
    };
    ax_check(code, "focus the element")
}

// ── focus ───────────────────────────────────────────────────────────────────

fn activate_app(pid: u32) -> CuaResult<ActOutcome> {
    let app = app_element(pid)?;
    set_app_frontmost(&app)?;
    let deadline = Instant::now() + FOCUS_WAIT;
    loop {
        let frontmost = cg_windows(false)
            .ok()
            .and_then(|windows| frontmost_pid(&windows));
        if frontmost == Some(pid) {
            let window = cg_windows(true)
                .ok()
                .and_then(|windows| windows.into_iter().find(|window| window.pid == pid))
                .map(|window| WindowInfo {
                    id: window.id,
                    pid: window.pid,
                    app: window.app,
                    title: window.title,
                    bounds: window.bounds,
                    on_screen: window.on_screen,
                    focused: false,
                });
            return Ok(ActOutcome {
                action: "activate_app".into(),
                target: format!("pid {pid}"),
                status: ActStatus::Applied,
                detail: "the window server reports this application frontmost".into(),
                window,
                frontmost_app: app_info(pid),
            });
        }
        if Instant::now() >= deadline {
            return Ok(ActOutcome {
                action: "activate_app".into(),
                target: format!("pid {pid}"),
                status: ActStatus::CalledOnly,
                detail: "the accessibility call succeeded but the application did not become \
                         frontmost within 1.5s; it may be on another Space or prompting"
                    .into(),
                window: None,
                frontmost_app: app_info(pid),
            });
        }
        std::thread::sleep(FOCUS_POLL);
    }
}

fn set_app_frontmost(app: &CfOwned) -> CuaResult<()> {
    let Some(attribute) = CfString::new("AXFrontmost") else {
        return Err(backend("could not create the AXFrontmost attribute name"));
    };
    let code = unsafe {
        AXUIElementSetAttributeValue(
            app.as_ptr(),
            attribute.as_ptr(),
            cf::kCFBooleanTrue as CFTypeRef,
        )
    };
    ax_check(code, "bring the application to the front")
}

fn focus_window(window: u64) -> CuaResult<ActOutcome> {
    let target = cg_window(window)?;
    let app = app_element(target.pid)?;
    let windows = ax_windows(&app)?;
    let ax_window =
        choose_ax_window(&windows, &target).ok_or_else(|| CuaError::WindowNotFound(window))?;
    raise_window(&app, ax_window)?;
    let applied = wait_for_focus(&target, FOCUS_WAIT);
    Ok(ActOutcome {
        action: "focus_window".into(),
        target: format!("{} {:?}", target.app, target.title),
        status: if applied {
            ActStatus::Applied
        } else {
            ActStatus::CalledOnly
        },
        detail: if applied {
            "the window server and the application both report this window focused".into()
        } else {
            "the application accepted the focus request but did not report the window \
             focused within 1.5s"
                .into()
        },
        window: window_info(window).ok(),
        frontmost_app: app_info(target.pid),
    })
}

/// Selects a window and brings its application forward.
///
/// `AXMain` is the attribute that actually changes which window the
/// application reports through `AXFocusedWindow`; `AXRaise` is kept as the
/// fallback for elements that do not carry `AXMain`.
fn raise_window(app: &CfOwned, window: &AxWindow) -> CuaResult<()> {
    let Some(main) = CfString::new("AXMain") else {
        return Err(backend("could not create the AXMain attribute name"));
    };
    let code = unsafe {
        AXUIElementSetAttributeValue(
            window.element.as_ptr(),
            main.as_ptr(),
            cf::kCFBooleanTrue as CFTypeRef,
        )
    };
    match code {
        0 => {}
        -25206 => raise_action(window)?,
        code => ax_check(code, "make the window main")?,
    }
    set_app_frontmost(app)
}

fn raise_action(window: &AxWindow) -> CuaResult<()> {
    let Some(action) = CfString::new("AXRaise") else {
        return Err(backend("could not create the AXRaise action name"));
    };
    let code = unsafe { AXUIElementPerformAction(window.element.as_ptr(), action.as_ptr()) };
    ax_check(code, "raise the window")
}

fn wait_for_focus(target: &CgWindow, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let frontmost = cg_windows(false)
            .ok()
            .and_then(|windows| frontmost_pid(&windows));
        if frontmost == Some(target.pid) && focused_window_id(&target.pid) == Some(target.id) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(FOCUS_POLL);
    }
}

fn ensure_focus(target: &CgWindow, activate: bool) -> CuaResult<()> {
    let frontmost = cg_windows(false)?;
    let frontmost_pid = frontmost_pid(&frontmost);
    if frontmost_pid == Some(target.pid) && focused_window_id(&target.pid) == Some(target.id) {
        return Ok(());
    }
    if !activate {
        let frontmost_name = frontmost_pid
            .and_then(|pid| frontmost.iter().find(|window| window.pid == pid))
            .map(|window| format!("{} {:?}", window.app, window.title))
            .unwrap_or_else(|| "another application".into());
        return Err(CuaError::FocusConflict {
            target: format!("{} {:?}", target.app, target.title),
            frontmost: frontmost_name,
        });
    }
    let app = app_element(target.pid)?;
    let windows = ax_windows(&app)?;
    let ax_window =
        choose_ax_window(&windows, target).ok_or_else(|| CuaError::WindowNotFound(target.id))?;
    raise_window(&app, ax_window)?;
    if wait_for_focus(target, FOCUS_WAIT) {
        Ok(())
    } else {
        Err(CuaError::FocusUnavailable(format!(
            "{} {:?} (pid {})",
            target.app, target.title, target.pid
        )))
    }
}

// ── actions ─────────────────────────────────────────────────────────────────

fn click(
    window: u64,
    path: Option<&[usize]>,
    point: Option<Point>,
    guard_bounds: Option<Bounds>,
    button: MouseButton,
    count: u8,
    action: &ResolvedAction,
) -> CuaResult<ActOutcome> {
    let target = cg_window(window)?;
    let before_title = target.title.clone();
    let app = app_element(target.pid)?;
    let windows = ax_windows(&app)?;
    let ax_window =
        choose_ax_window(&windows, &target).ok_or_else(|| CuaError::WindowNotFound(window))?;

    let mut detail;
    if let Some(path) = path {
        let element = resolve_element(ax_window, path, action.expectation.as_ref())?;
        if count == 1
            && ax_actions(element.as_ptr())
                .iter()
                .any(|name| name == "AXPress")
        {
            let action_name = CfString::new("AXPress")
                .ok_or_else(|| backend("could not create the AXPress action name"))?;
            let code = unsafe { AXUIElementPerformAction(element.as_ptr(), action_name.as_ptr()) };
            ax_check(code, "press the element")?;
            detail = "AXPress was accepted".to_string();
        } else {
            let bounds = ax_bounds(element.as_ptr()).ok_or_else(|| {
                CuaError::ElementChanged("the element has no bounds to click".into())
            })?;
            ensure_focus(&target, action.activate)?;
            input::post_click(bounds.center(), button, count)?;
            detail = format!(
                "posted {} click(s) at the element center ({:.1}, {:.1})",
                count,
                bounds.center().x,
                bounds.center().y
            );
        }
    } else {
        let point = point.ok_or_else(|| CuaError::InvalidAction("click needs a point".into()))?;
        guard_window_bounds(&target, guard_bounds)?;
        ensure_focus(&target, action.activate)?;
        input::post_click(point, button, count)?;
        detail = format!(
            "posted {} click(s) at ({:.1}, {:.1})",
            count, point.x, point.y
        );
    }

    let after = cg_window(window).ok();
    let after_title = after.as_ref().map(|window| window.title.clone());
    if after_title.as_deref() != Some(before_title.as_str()) {
        detail.push_str("; the window title changed");
    }
    Ok(outcome(
        "click",
        &target,
        ActStatus::CalledOnly,
        detail,
        after,
    ))
}

fn type_text(
    window: u64,
    path: Option<&[usize]>,
    text: &str,
    submit: bool,
    expectation: Option<&Expectation>,
    activate: bool,
) -> CuaResult<ActOutcome> {
    let target = cg_window(window)?;
    let app = app_element(target.pid)?;
    let windows = ax_windows(&app)?;
    let ax_window =
        choose_ax_window(&windows, &target).ok_or_else(|| CuaError::WindowNotFound(window))?;

    let mut detail;
    let mut applied = false;
    if let Some(path) = path {
        let element = resolve_element(ax_window, path, expectation)?;
        match set_ax_value(&element, text) {
            Ok(()) => {
                let live = ax_value_string(element.as_ptr());
                applied = live.as_deref() == Some(text);
                detail = if applied {
                    "AXValue was set and read back exactly".into()
                } else {
                    "AXValue was set but the read-back value differs".to_string()
                };
            }
            Err(_) => {
                set_ax_focused(&element)?;
                ensure_focus(&target, activate)?;
                input::post_text(text)?;
                detail = "the element refused AXValue; text was posted as keyboard input".into();
            }
        }
    } else {
        ensure_focus(&target, activate)?;
        input::post_text(text)?;
        detail = "text was posted as keyboard input".into();
    }
    if submit {
        input::post_key("return", &[])?;
        detail.push_str("; Return was posted");
    }
    let after = cg_window(window).ok();
    Ok(outcome(
        "type",
        &target,
        if applied {
            ActStatus::Applied
        } else {
            ActStatus::CalledOnly
        },
        detail,
        after,
    ))
}

fn post_key(
    window: u64,
    key: &str,
    modifiers: &[Modifier],
    activate: bool,
) -> CuaResult<ActOutcome> {
    let target = cg_window(window)?;
    ensure_focus(&target, activate)?;
    input::post_key(key, modifiers)?;
    let after = cg_window(window).ok();
    Ok(outcome(
        "key",
        &target,
        ActStatus::CalledOnly,
        format!("posted key {key:?} with {} modifier(s)", modifiers.len()),
        after,
    ))
}

fn scroll(
    window: u64,
    point: Option<Point>,
    guard_bounds: Option<Bounds>,
    dx: f64,
    dy: f64,
    activate: bool,
) -> CuaResult<ActOutcome> {
    let target = cg_window(window)?;
    if let Some(guard) = guard_bounds {
        guard_window_bounds(&target, Some(guard))?;
    }
    ensure_focus(&target, activate)?;
    let point = point.unwrap_or_else(|| target.bounds.center());
    input::post_scroll(point, dx, dy)?;
    let after = cg_window(window).ok();
    Ok(outcome(
        "scroll",
        &target,
        ActStatus::CalledOnly,
        format!(
            "posted scroll dx={dx} dy={dy} at ({:.1}, {:.1})",
            point.x, point.y
        ),
        after,
    ))
}

fn set_value(
    window: u64,
    path: &[usize],
    value: &str,
    expectation: Option<&Expectation>,
) -> CuaResult<ActOutcome> {
    let target = cg_window(window)?;
    let app = app_element(target.pid)?;
    let windows = ax_windows(&app)?;
    let ax_window =
        choose_ax_window(&windows, &target).ok_or_else(|| CuaError::WindowNotFound(window))?;
    let element = resolve_element(ax_window, path, expectation)?;
    set_ax_value(&element, value)?;
    let live = ax_value_string(element.as_ptr());
    let (status, detail) = match live {
        Some(live) if live == value => (
            ActStatus::Applied,
            "AXValue was read back exactly".to_string(),
        ),
        Some(_) => (
            ActStatus::Unchanged,
            "AXValue was accepted but the read-back value differs".to_string(),
        ),
        None => (
            ActStatus::Unknown,
            "AXValue was accepted but this element exposes no readable value".to_string(),
        ),
    };
    let after = cg_window(window).ok();
    Ok(outcome("set_value", &target, status, detail, after))
}

fn perform_action(
    window: u64,
    path: &[usize],
    action_name: &str,
    expectation: Option<&Expectation>,
) -> CuaResult<ActOutcome> {
    let target = cg_window(window)?;
    let before_title = target.title.clone();
    let app = app_element(target.pid)?;
    let windows = ax_windows(&app)?;
    let ax_window =
        choose_ax_window(&windows, &target).ok_or_else(|| CuaError::WindowNotFound(window))?;
    let element = resolve_element(ax_window, path, expectation)?;
    let actions = ax_actions(element.as_ptr());
    if !actions.iter().any(|name| name == action_name) {
        return Err(CuaError::InvalidAction(format!(
            "the element supports {:?}, not {action_name:?}",
            actions
        )));
    }
    let name = CfString::new(action_name)
        .ok_or_else(|| backend("could not create the accessibility action name"))?;
    let code = unsafe { AXUIElementPerformAction(element.as_ptr(), name.as_ptr()) };
    ax_check(code, "perform the accessibility action")?;
    let after = cg_window(window).ok();
    let after_title = after.as_ref().map(|window| window.title.clone());
    let changed = after_title.as_deref() != Some(before_title.as_str())
        || after.is_none()
        || resolve_element(ax_window, path, expectation).is_err();
    Ok(outcome(
        "perform_action",
        &target,
        if changed {
            ActStatus::Applied
        } else {
            ActStatus::CalledOnly
        },
        if changed {
            format!("{action_name} was accepted and the window changed")
        } else {
            format!("{action_name} was accepted; no observable change yet")
        },
        after,
    ))
}

fn guard_window_bounds(target: &CgWindow, observed: Option<Bounds>) -> CuaResult<()> {
    if let Some(observed) = observed
        && !target.bounds.approximately_equals(&observed)
    {
        return Err(CuaError::WindowMoved {
            window: target.id,
            observed,
            current: target.bounds,
        });
    }
    Ok(())
}

fn outcome(
    action: &str,
    target: &CgWindow,
    status: ActStatus,
    detail: String,
    after: Option<CgWindow>,
) -> ActOutcome {
    ActOutcome {
        action: action.to_string(),
        target: format!("{} {:?}", target.app, target.title),
        status,
        detail,
        window: after.map(|window| {
            let focused = focused_window_id(&window.pid) == Some(window.id);
            WindowInfo {
                id: window.id,
                pid: window.pid,
                app: window.app,
                title: window.title,
                bounds: window.bounds,
                on_screen: window.on_screen,
                focused,
            }
        }),
        frontmost_app: app_info(target.pid),
    }
}

fn backend(detail: impl Into<String>) -> CuaError {
    CuaError::Backend {
        backend: NAME,
        detail: detail.into(),
    }
}
