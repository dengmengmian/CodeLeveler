//! Windows backend: UI Automation for semantics and trees, Win32 for window
//! enumeration and input, GDI for pixels.
//!
//! The backend never claims that an action "completed". It reports what the
//! platform accepted and what a fresh observation saw. It also never types an
//! elevation path: a target at a higher integrity level is refused with
//! [`CuaError::ElevationRequired`] (UIPI), not worked around.

mod capture;
mod input;
mod uia;

use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
    DESKTOP_SWITCHDESKTOP, OpenInputDesktop,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SW_RESTORE, SetForegroundWindow, ShowWindowAsync, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PWSTR};

use crate::backend::Backend;
use crate::error::{CuaError, CuaResult};
use crate::types::{
    ActOutcome, ActStatus, ActionKind, AppInfo, Availability, Bounds, Expectation, Modifier,
    MouseButton, Node, PermissionState, Point, ResolvedAction, Screenshot, WindowInfo,
};

pub(crate) const NAME: &str = "windows";

const FOCUS_WAIT: Duration = Duration::from_millis(1500);
const FOCUS_POLL: Duration = Duration::from_millis(50);

pub(crate) fn init_thread() {
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if result.is_err() {
        tracing::error!(
            code = format!("0x{:08X}", result.0),
            "could not initialize COM on the desktop worker thread"
        );
    }
}

pub(crate) fn shutdown_thread() {
    unsafe { CoUninitialize() };
}

pub(crate) fn new_backend() -> Box<dyn Backend> {
    Box::new(WinBackend)
}

struct WinBackend;

impl Backend for WinBackend {
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
        let accessibility = match uia::automation() {
            Ok(_) => PermissionState::Granted,
            Err(error) => {
                notes.push(format!("UI Automation is unavailable: {error}"));
                PermissionState::Unsupported
            }
        };
        notes.push(
            "Windows has no screen-capture grant; protected content can still render blank".into(),
        );
        notes.push(
            "Command is treated as Control; an elevated application rejects input from a \
             non-elevated process (UIPI)"
                .into(),
        );
        Ok(Availability {
            platform: NAME.to_string(),
            supported: true,
            interactive_session: interactive,
            accessibility,
            screen_recording: PermissionState::Granted,
            notes,
        })
    }

    fn apps(&mut self) -> CuaResult<Vec<AppInfo>> {
        interactive_session()?;
        let windows = collect_windows();
        let foreground = foreground_pid();
        let mut apps: Vec<AppInfo> = Vec::new();
        for window in &windows {
            if let Some(existing) = apps.iter_mut().find(|app| app.pid == window.pid) {
                existing.window_count += 1;
                continue;
            }
            apps.push(AppInfo {
                pid: window.pid,
                name: window.app.clone(),
                window_count: 1,
                frontmost: Some(window.pid) == foreground,
            });
        }
        Ok(apps)
    }

    fn windows(&mut self, app: Option<&str>) -> CuaResult<Vec<WindowInfo>> {
        interactive_session()?;
        let foreground = foreground_hwnd();
        let filter = app.map(str::trim).filter(|value| !value.is_empty());
        Ok(collect_windows()
            .into_iter()
            .filter(|window| app_matches(filter, window))
            .map(|window| WindowInfo {
                id: window.id(),
                pid: window.pid,
                app: window.app.clone(),
                title: window.title.clone(),
                bounds: window.bounds,
                on_screen: window.on_screen,
                focused: foreground == Some(window.hwnd),
            })
            .collect())
    }

    fn tree(
        &mut self,
        window: u64,
        depth: u32,
        max_nodes: u32,
        cancel: &AtomicBool,
    ) -> CuaResult<(WindowInfo, Node, bool)> {
        interactive_session()?;
        let target = target_window(window)?;
        let ui = Ui::new()?;
        let root = uia::root(&ui.automation, target.hwnd)?;
        let mut walk = uia::Walk::new(&ui.walker, max_nodes, depth, cancel);
        let node = walk.node(&root, Vec::new(), 0)?;
        Ok((target.info(), node, walk.truncated))
    }

    fn screenshot(&mut self, window: Option<u64>) -> CuaResult<Screenshot> {
        interactive_session()?;
        let info = match window {
            Some(window) => Some(target_window(window)?.info()),
            None => None,
        };
        capture::capture(info.as_ref())
    }

    fn act(&mut self, action: &ResolvedAction) -> CuaResult<ActOutcome> {
        interactive_session()?;
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

// ── session ─────────────────────────────────────────────────────────────────

fn interactive_session() -> CuaResult<()> {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session) }.map_err(|error| {
        backend(format!(
            "could not read this process's session: {}",
            error.message()
        ))
    })?;
    let active = unsafe { WTSGetActiveConsoleSessionId() };
    if active == u32::MAX {
        return Err(CuaError::NoInteractiveSession(
            "there is no active console session".into(),
        ));
    }
    if session != active {
        return Err(CuaError::NoInteractiveSession(format!(
            "this process runs in session {session}, but the console session is {active}"
        )));
    }
    let access = DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS.0 | DESKTOP_SWITCHDESKTOP.0);
    match unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, access) } {
        Ok(desktop) => {
            unsafe {
                let _ = CloseDesktop(desktop);
            }
            Ok(())
        }
        Err(error) => Err(CuaError::NoInteractiveSession(format!(
            "the input desktop is not accessible: {}; the workstation may be locked or on a \
             secure desktop",
            error.message()
        ))),
    }
}

// ── window enumeration ──────────────────────────────────────────────────────

#[derive(Clone)]
struct WinWindow {
    hwnd: HWND,
    pid: u32,
    app: String,
    title: String,
    bounds: Bounds,
    on_screen: bool,
}

impl WinWindow {
    fn id(&self) -> u64 {
        self.hwnd.0 as usize as u64
    }

    fn info(&self) -> WindowInfo {
        WindowInfo {
            id: self.id(),
            pid: self.pid,
            app: self.app.clone(),
            title: self.title.clone(),
            bounds: self.bounds,
            on_screen: self.on_screen,
            focused: foreground_hwnd() == Some(self.hwnd),
        }
    }
}

unsafe extern "system" fn enumerate_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let handles = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    handles.push(hwnd);
    BOOL(1)
}

fn collect_windows() -> Vec<WinWindow> {
    let mut handles: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enumerate_window),
            LPARAM(&mut handles as *mut Vec<HWND> as isize),
        );
    }
    handles.into_iter().filter_map(describe_window).collect()
}

fn describe_window(hwnd: HWND) -> Option<WinWindow> {
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return None;
    }
    let style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    if style & WS_EX_TOOLWINDOW.0 != 0 {
        return None;
    }
    if is_cloaked(hwnd) {
        return None;
    }
    let pid = pid_of(hwnd)?;
    let bounds = window_bounds(hwnd)?;
    let title = window_title(hwnd);
    let mut app = process_name(pid);
    if app.is_empty() {
        app = window_class(hwnd);
    }
    let on_screen = !unsafe { IsIconic(hwnd) }.as_bool();
    Some(WinWindow {
        hwnd,
        pid,
        app,
        title,
        bounds,
        on_screen,
    })
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let result = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        )
    };
    result.is_ok() && cloaked != 0
}

fn window_bounds(hwnd: HWND) -> Option<Bounds> {
    let mut rect = RECT::default();
    let dwm = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
    };
    if dwm.is_err() {
        unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    }
    let width = f64::from(rect.right - rect.left);
    let height = f64::from(rect.bottom - rect.top);
    (width > 1.0 && height > 1.0).then_some(Bounds {
        x: f64::from(rect.left),
        y: f64::from(rect.top),
        width,
        height,
    })
}

fn window_title(hwnd: HWND) -> String {
    let length = unsafe { GetWindowTextLengthW(hwnd) };
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..copied as usize])
}

fn window_class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let copied = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..copied as usize])
}

fn process_name(pid: u32) -> String {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        );
        let _ = windows::Win32::Foundation::CloseHandle(process);
        if result.is_err() {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buffer[..size as usize]);
        path.rsplit(['\\', '/'])
            .next()
            .unwrap_or(&path)
            .trim_end_matches(".exe")
            .to_string()
    }
}

fn foreground_hwnd() -> Option<HWND> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.0.is_null()).then_some(hwnd)
}

fn foreground_pid() -> Option<u32> {
    foreground_hwnd().and_then(pid_of)
}

fn pid_of(hwnd: HWND) -> Option<u32> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

fn app_matches(filter: Option<&str>, window: &WinWindow) -> bool {
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

pub(super) fn hwnd(id: u64) -> HWND {
    HWND(id as usize as *mut c_void)
}

fn target_window(id: u64) -> CuaResult<WinWindow> {
    collect_windows()
        .into_iter()
        .find(|window| window.id() == id)
        .ok_or(CuaError::WindowNotFound(id))
}

fn label(target: &WinWindow) -> String {
    format!("{} {:?}", target.app, target.title)
}

fn app_info(pid: u32) -> Option<AppInfo> {
    let windows = collect_windows();
    let window = windows.iter().find(|window| window.pid == pid)?;
    Some(AppInfo {
        pid,
        name: window.app.clone(),
        window_count: windows.iter().filter(|window| window.pid == pid).count(),
        frontmost: foreground_pid() == Some(pid),
    })
}

// ── UI Automation glue ──────────────────────────────────────────────────────

struct Ui {
    automation: IUIAutomation,
    walker: IUIAutomationTreeWalker,
}

impl Ui {
    fn new() -> CuaResult<Self> {
        let automation = uia::automation()?;
        let walker = uia::walker(&automation)?;
        Ok(Self { automation, walker })
    }

    fn element(
        &self,
        window: u64,
        path: &[usize],
        expectation: Option<&Expectation>,
    ) -> CuaResult<IUIAutomationElement> {
        let root = uia::root(&self.automation, hwnd(window))?;
        uia::resolve(&self.walker, &root, path, expectation)
    }
}

// ── focus ───────────────────────────────────────────────────────────────────

fn focus_hwnd(target: &WinWindow) -> CuaResult<()> {
    if foreground_hwnd() == Some(target.hwnd) {
        return Ok(());
    }
    unsafe {
        if IsIconic(target.hwnd).as_bool() {
            let _ = ShowWindowAsync(target.hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(target.hwnd);
    }
    Ok(())
}

fn wait_for_foreground(hwnd: HWND, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if foreground_hwnd() == Some(hwnd) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(FOCUS_POLL);
    }
}

fn wait_for_foreground_pid(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if foreground_pid() == Some(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(FOCUS_POLL);
    }
}

fn ensure_focus(target: &WinWindow, activate: bool) -> CuaResult<()> {
    if foreground_hwnd() == Some(target.hwnd) {
        return Ok(());
    }
    if !activate {
        let frontmost = foreground_hwnd()
            .map(describe_foreground)
            .unwrap_or_else(|| "another application".into());
        return Err(CuaError::FocusConflict {
            target: label(target),
            frontmost,
        });
    }
    focus_hwnd(target)?;
    if wait_for_foreground(target.hwnd, FOCUS_WAIT) {
        Ok(())
    } else {
        Err(CuaError::FocusUnavailable(label(target)))
    }
}

fn describe_foreground(hwnd: HWND) -> String {
    let title = window_title(hwnd);
    let mut app = pid_of(hwnd).map(process_name).unwrap_or_default();
    if app.is_empty() {
        app = window_class(hwnd);
    }
    format!("{app} {title:?}")
}

fn activate_app(pid: u32) -> CuaResult<ActOutcome> {
    let target = collect_windows()
        .into_iter()
        .find(|window| window.pid == pid)
        .ok_or_else(|| {
            CuaError::AppNotFound(format!("pid {pid} has no visible top-level window"))
        })?;
    focus_hwnd(&target)?;
    let applied = wait_for_foreground_pid(pid, FOCUS_WAIT);
    Ok(ActOutcome {
        action: "activate_app".into(),
        target: format!("pid {pid}"),
        status: if applied {
            ActStatus::Applied
        } else {
            ActStatus::CalledOnly
        },
        detail: if applied {
            "the foreground window belongs to this application".into()
        } else {
            "the window was raised but the foreground did not move within 1.5s; Windows may \
             refuse a background process, or another window holds the foreground lock"
                .into()
        },
        window: applied.then(|| target.info()),
        frontmost_app: applied.then(|| app_info(pid)).flatten(),
    })
}

fn focus_window(window: u64) -> CuaResult<ActOutcome> {
    let target = target_window(window)?;
    focus_hwnd(&target)?;
    let applied = wait_for_foreground(target.hwnd, FOCUS_WAIT);
    Ok(ActOutcome {
        action: "focus_window".into(),
        target: label(&target),
        status: if applied {
            ActStatus::Applied
        } else {
            ActStatus::CalledOnly
        },
        detail: if applied {
            "the foreground window is the requested window".into()
        } else {
            "the window was raised but did not become the foreground window within 1.5s".into()
        },
        window: target_window(window).ok().map(|window| window.info()),
        frontmost_app: app_info(target.pid),
    })
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
    let target = target_window(window)?;
    let before_title = target.title.clone();
    let mut detail = if let Some(path) = path {
        let ui = Ui::new()?;
        let element = ui.element(window, path, action.expectation.as_ref())?;
        let patterns = uia::Patterns::read(&element);
        let semantic = count == 1 && matches!(button, MouseButton::Left);
        if semantic && patterns.invoke.is_some() {
            patterns.perform("invoke")?;
            "the Invoke pattern was accepted".to_string()
        } else if semantic && patterns.toggle.is_some() {
            patterns.perform("toggle")?;
            "the Toggle pattern was accepted".to_string()
        } else if semantic && patterns.selection.is_some() {
            patterns.perform("select")?;
            "the Select pattern was accepted".to_string()
        } else {
            coordinate_click(
                &target,
                uia::element_bounds(&element),
                button,
                count,
                action.activate,
            )?
        }
    } else {
        let point = point.ok_or_else(|| CuaError::InvalidAction("click needs a point".into()))?;
        guard_window_bounds(&target, guard_bounds)?;
        ensure_focus(&target, action.activate)?;
        input::click(point, button, count)?;
        format!(
            "posted {count} click(s) at ({:.1}, {:.1})",
            point.x, point.y
        )
    };
    let after = target_window(window).ok();
    if after.as_ref().map(|window| window.title.as_str()) != Some(before_title.as_str()) {
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

fn coordinate_click(
    target: &WinWindow,
    bounds: Option<Bounds>,
    button: MouseButton,
    count: u8,
    activate: bool,
) -> CuaResult<String> {
    let bounds = bounds
        .ok_or_else(|| CuaError::ElementChanged("the element has no bounds to click".into()))?;
    ensure_focus(target, activate)?;
    let point = bounds.center();
    input::click(point, button, count)?;
    Ok(format!(
        "posted {count} click(s) at the element center ({:.1}, {:.1})",
        point.x, point.y
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
    let target = target_window(window)?;
    let mut applied = false;
    let mut detail;
    if let Some(path) = path {
        let ui = Ui::new()?;
        let element = ui.element(window, path, expectation)?;
        let patterns = uia::Patterns::read(&element);
        if patterns.writable_value().is_some() {
            match patterns.set_value(text) {
                Ok(()) => {
                    applied = patterns.raw_value_text().as_deref() == Some(text);
                    detail = if applied {
                        "the value pattern accepted the text and read it back exactly".into()
                    } else {
                        "the value pattern accepted the text but the read-back value differs"
                            .to_string()
                    };
                }
                Err(_) => {
                    ensure_focus(&target, activate)?;
                    uia::focus(&element)?;
                    input::text(text)?;
                    detail = "the value pattern refused the text; it was posted as keyboard input"
                        .into();
                }
            }
        } else {
            ensure_focus(&target, activate)?;
            uia::focus(&element)?;
            input::text(text)?;
            detail =
                "the element exposes no writable value; text was posted as keyboard input".into();
        }
        if submit {
            ensure_focus(&target, activate)?;
            uia::focus(&element)?;
            input::press("return", &[])?;
            detail.push_str("; Return was posted");
        }
    } else {
        ensure_focus(&target, activate)?;
        input::text(text)?;
        detail = "text was posted as keyboard input".into();
        if submit {
            input::press("return", &[])?;
            detail.push_str("; Return was posted");
        }
    }
    Ok(outcome(
        "type",
        &target,
        if applied {
            ActStatus::Applied
        } else {
            ActStatus::CalledOnly
        },
        detail,
        target_window(window).ok(),
    ))
}

fn post_key(
    window: u64,
    key: &str,
    modifiers: &[Modifier],
    activate: bool,
) -> CuaResult<ActOutcome> {
    let target = target_window(window)?;
    ensure_focus(&target, activate)?;
    input::press(key, modifiers)?;
    Ok(outcome(
        "key",
        &target,
        ActStatus::CalledOnly,
        format!("posted key {key:?} with {} modifier(s)", modifiers.len()),
        target_window(window).ok(),
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
    let target = target_window(window)?;
    if let Some(guard) = guard_bounds {
        guard_window_bounds(&target, Some(guard))?;
    }
    ensure_focus(&target, activate)?;
    let point = point.unwrap_or_else(|| target.bounds.center());
    input::scroll(point, dx, dy)?;
    Ok(outcome(
        "scroll",
        &target,
        ActStatus::CalledOnly,
        format!(
            "posted scroll dx={dx} dy={dy} at ({:.1}, {:.1})",
            point.x, point.y
        ),
        target_window(window).ok(),
    ))
}

fn set_value(
    window: u64,
    path: &[usize],
    value: &str,
    expectation: Option<&Expectation>,
) -> CuaResult<ActOutcome> {
    let target = target_window(window)?;
    let ui = Ui::new()?;
    let element = ui.element(window, path, expectation)?;
    let patterns = uia::Patterns::read(&element);
    patterns.set_value(value)?;
    let live = patterns.raw_value_text();
    let (status, detail) = match live {
        Some(live) if live == value => (
            ActStatus::Applied,
            "the value was read back exactly".to_string(),
        ),
        Some(_) => (
            ActStatus::Unchanged,
            "the value pattern accepted the new value but the read-back differs".to_string(),
        ),
        None => (
            ActStatus::Unknown,
            "the value pattern accepted the new value but exposes no readable value".to_string(),
        ),
    };
    Ok(outcome(
        "set_value",
        &target,
        status,
        detail,
        target_window(window).ok(),
    ))
}

fn perform_action(
    window: u64,
    path: &[usize],
    action_name: &str,
    expectation: Option<&Expectation>,
) -> CuaResult<ActOutcome> {
    let target = target_window(window)?;
    let ui = Ui::new()?;
    let element = ui.element(window, path, expectation)?;
    let patterns = uia::Patterns::read(&element);
    let actions = patterns.action_names();
    if !actions.iter().any(|action| action == action_name) {
        return Err(CuaError::InvalidAction(format!(
            "the element supports {actions:?}, not {action_name:?}"
        )));
    }
    patterns.perform(action_name)?;
    Ok(outcome(
        "perform_action",
        &target,
        ActStatus::CalledOnly,
        format!("the {action_name} pattern was accepted"),
        target_window(window).ok(),
    ))
}

fn guard_window_bounds(target: &WinWindow, observed: Option<Bounds>) -> CuaResult<()> {
    if let Some(observed) = observed
        && !target.bounds.approximately_equals(&observed)
    {
        return Err(CuaError::WindowMoved {
            window: target.id(),
            observed,
            current: target.bounds,
        });
    }
    Ok(())
}

fn outcome(
    action: &str,
    target: &WinWindow,
    status: ActStatus,
    detail: String,
    after: Option<WinWindow>,
) -> ActOutcome {
    ActOutcome {
        action: action.to_string(),
        target: label(target),
        status,
        detail,
        window: after.map(|window| window.info()),
        frontmost_app: app_info(target.pid),
    }
}

fn backend(detail: impl Into<String>) -> CuaError {
    CuaError::Backend {
        backend: NAME,
        detail: detail.into(),
    }
}
