//! Native desktop observation and control.
//!
//! Two tools, because looking and acting are different authorities:
//! `computer_observe` reads the screen and the accessibility tree,
//! `computer_act` sends input. Both are thin adapters over
//! [`leveler_cua::ComputerUse`], which owns the platform backend, the
//! observation generation and the element references. Neither tool carries a
//! loop, a plan, an approval decision or a screenshot log.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_cua::{
    ActOutcome, ActRequest, ActStatus, Availability, ComputerUse, ElementInfo, Modifier,
    MouseButton, Observation, Point, WindowInfo,
};
use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_DEPTH: u32 = 4;
const DEFAULT_MAX_NODES: u32 = 300;

// ── computer_observe ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ObserveAction {
    /// Report platform support, the interactive session and TCC grants.
    Availability,
    /// List applications that own a window.
    Apps,
    /// List windows, optionally filtered by application name or pid.
    Windows,
    /// Read one window's accessibility tree and mint element references.
    Tree,
    /// Capture a window or the main display as an image.
    Screenshot,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ObserveInput {
    /// What to observe.
    action: ObserveAction,
    /// `windows`: filter by application name (substring) or pid.
    #[serde(default)]
    app: Option<String>,
    /// `tree` / `screenshot`: window id from `windows`.
    #[serde(default)]
    window: Option<u64>,
    /// `tree`: how many levels below the window to read (1-8, default 4).
    #[serde(default)]
    depth: Option<u32>,
    /// `tree`: maximum elements to return (20-800, default 300).
    #[serde(default)]
    max_nodes: Option<u32>,
}

pub struct ComputerObserveTool {
    computer: Arc<ComputerUse>,
}

impl ComputerObserveTool {
    pub fn new(computer: Arc<ComputerUse>) -> Self {
        Self { computer }
    }
}

#[async_trait]
impl Tool for ComputerObserveTool {
    fn name(&self) -> &'static str {
        "computer_observe"
    }

    fn description(&self) -> &'static str {
        "Observe the desktop: list applications and windows, read a window's \
         accessibility tree (returns element references for computer_act), or \
         capture a window/display screenshot. Coordinates are logical screen \
         points; for an image, divide image pixels by the reported scale."
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ObserveInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn replay_is_side_effect_free(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: ObserveInput = super::parse_input(self.name(), input)?;
        let scope = context.session_scope().to_string();
        let computer = self.computer.clone();
        match input.action {
            ObserveAction::Availability => {
                match blocking_raw(move || computer.availability()).await {
                    Ok(availability) => Ok(ToolOutput::ok(describe_availability(&availability))),
                    Err(error) => Ok(ToolOutput::error(error.to_string())),
                }
            }
            ObserveAction::Apps => match blocking_raw(move || computer.apps()).await {
                Ok(apps) => {
                    let mut text = format!("{} applications with windows\n", apps.len());
                    for app in &apps {
                        let _ = writeln!(
                            text,
                            "pid={} {:?} windows={}{}",
                            app.pid,
                            app.name,
                            app.window_count,
                            if app.frontmost { " frontmost" } else { "" }
                        );
                    }
                    Ok(ToolOutput::ok(text))
                }
                Err(error) => Ok(ToolOutput::error(error.to_string())),
            },
            ObserveAction::Windows => {
                let app = input.app.clone();
                match blocking_raw(move || computer.windows(&scope, app.as_deref())).await {
                    Ok(windows) => Ok(ToolOutput::ok(describe_windows(
                        &windows,
                        input.app.as_deref(),
                    ))),
                    Err(error) => Ok(ToolOutput::error(error.to_string())),
                }
            }
            ObserveAction::Tree => {
                let Some(window) = input.window else {
                    return Ok(ToolOutput::error(
                        "computer_observe tree needs `window` from an observation",
                    ));
                };
                let depth = input.depth.unwrap_or(DEFAULT_DEPTH);
                let max_nodes = input.max_nodes.unwrap_or(DEFAULT_MAX_NODES);
                let cancel = Arc::new(AtomicBool::new(false));
                let watcher = spawn_cancel_bridge(&cancellation, cancel.clone());
                let observation = blocking_raw(move || {
                    computer.observe_window(&scope, window, depth, max_nodes, Some(cancel))
                })
                .await;
                watcher.abort();
                match observation {
                    Ok(observation) => Ok(ToolOutput::ok(describe_tree(&observation))),
                    Err(error) => Ok(ToolOutput::error(error.to_string())),
                }
            }
            ObserveAction::Screenshot => {
                let window = input.window;
                let screenshot = match blocking_raw(move || computer.screenshot(window)).await {
                    Ok(screenshot) => screenshot,
                    Err(error) => return Ok(ToolOutput::error(error.to_string())),
                };
                let png = screenshot.png.clone();
                let processed =
                    tokio::task::spawn_blocking(move || leveler_media::process_image(&png))
                        .await
                        .map_err(|error| {
                            ToolError::Io(format!("image processing failed: {error}"))
                        })?
                        .map_err(|error| {
                            ToolError::Io(format!("image processing failed: {error}"))
                        })?;
                let logical_width = if screenshot.scale > 0.0 {
                    f64::from(screenshot.width) / screenshot.scale
                } else {
                    f64::from(screenshot.width)
                };
                let scale = if logical_width > 0.0 {
                    f64::from(processed.width) / logical_width
                } else {
                    1.0
                };
                let mut text = format!(
                    "captured {}\nimage: {}x{} px ({} KB)",
                    screenshot.target,
                    processed.width,
                    processed.height,
                    processed.png.len() / 1024
                );
                if processed.width != screenshot.width {
                    let _ = write!(
                        text,
                        "\ndownscaled from {}x{} px",
                        screenshot.width, screenshot.height
                    );
                }
                if let Some(window) = &screenshot.window {
                    let _ = write!(
                        text,
                        "\nwindow: id={} app={:?} title={:?} bounds={}",
                        window.id,
                        window.app,
                        window.title,
                        describe_bounds(window)
                    );
                }
                let _ = write!(text, "\nscale: {scale:.4} image pixels per screen point");
                let data = base64_encode(&processed.png);
                Ok(ToolOutput::ok(text).with_metadata(serde_json::json!({
                    "image": {
                        "media_type": leveler_media::PROCESSED_MIME,
                        "data": data,
                    }
                })))
            }
        }
    }
}

// ── computer_act ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ActAction {
    /// Bring an application to the front.
    ActivateApp,
    /// Raise one window and give it keyboard focus.
    FocusWindow,
    /// Click an element reference, or a screen point in an observed window.
    Click,
    /// Type into an element reference, or into the focused window.
    Type,
    /// Press one key with optional modifiers in the focused window.
    Key,
    /// Scroll the focused window at a point (default: window center).
    Scroll,
    /// Set an accessibility value directly.
    SetValue,
    /// Run one of the element's advertised accessibility actions.
    PerformAction,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Button {
    #[default]
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ModifierInput {
    Command,
    Control,
    Alt,
    Shift,
}

impl From<ModifierInput> for Modifier {
    fn from(value: ModifierInput) -> Self {
        match value {
            ModifierInput::Command => Self::Command,
            ModifierInput::Control => Self::Control,
            ModifierInput::Alt => Self::Alt,
            ModifierInput::Shift => Self::Shift,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ActInput {
    /// What to do.
    action: ActAction,
    /// `activate_app`: application pid from `computer_observe windows`.
    #[serde(default)]
    pid: Option<u32>,
    /// The window this action belongs to. Required for raw input (`key`,
    /// `scroll`, point `click`); optional when a reference names it.
    #[serde(default)]
    window: Option<u64>,
    /// Element reference from `computer_observe tree`.
    #[serde(default)]
    reference: Option<String>,
    /// `click`: screen-point x. Must be used with `y`, not with `reference`.
    #[serde(default)]
    x: Option<f64>,
    /// `click`: screen-point y.
    #[serde(default)]
    y: Option<f64>,
    /// `click`: mouse button.
    #[serde(default)]
    button: Button,
    /// `click`: click count (1-3).
    #[serde(default = "one")]
    count: u8,
    /// `type`: text to enter.
    #[serde(default)]
    text: Option<String>,
    /// `type`: press Return after the text.
    #[serde(default)]
    submit: bool,
    /// `key`: key name (letters, digits, punctuation, return, tab, escape,
    /// arrows, home, end, pageup, pagedown, f1..f12).
    #[serde(default)]
    key: Option<String>,
    /// `key`: modifiers held while the key is pressed.
    #[serde(default)]
    modifiers: Vec<ModifierInput>,
    /// `scroll`: horizontal lines (positive scrolls right).
    #[serde(default)]
    dx: Option<f64>,
    /// `scroll`: vertical lines (positive scrolls down).
    #[serde(default)]
    dy: Option<f64>,
    /// `set_value`: the new accessibility value.
    #[serde(default)]
    value: Option<String>,
    /// `perform_action`: an action name the element advertises.
    #[serde(default)]
    accessibility_action: Option<String>,
    /// Permit raising the window when another application is frontmost.
    /// Without it, raw input refuses a focus conflict instead of stealing
    /// focus. Semantic actions (AXPress, AXValue) do not need it.
    #[serde(default)]
    activate: bool,
}

fn one() -> u8 {
    1
}

pub struct ComputerActTool {
    computer: Arc<ComputerUse>,
}

impl ComputerActTool {
    pub fn new(computer: Arc<ComputerUse>) -> Self {
        Self { computer }
    }
}

#[async_trait]
impl Tool for ComputerActTool {
    fn name(&self) -> &'static str {
        "computer_act"
    }

    fn description(&self) -> &'static str {
        "Act on the native desktop: activate an application, focus a window, \
         click or type at an element reference, press keys, scroll, or set an \
         accessibility value. Prefer a reference over coordinates. Raw input \
         targets the focused window; pass activate=true to bring the target \
         forward when another application is frontmost. The result reports \
         what the platform accepted and what a fresh observation verified."
    }

    fn requires_workspace(&self) -> bool {
        false
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<ActInput>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Destructive
    }

    fn approval_reason(&self, input: &serde_json::Value, context: &ToolContext) -> Option<String> {
        let action = input
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("desktop action");
        let window = input.get("window").and_then(serde_json::Value::as_u64);
        let reference = input.get("reference").and_then(serde_json::Value::as_str);
        let pid = input.get("pid").and_then(serde_json::Value::as_u64);
        let target = self
            .computer
            .target_label(context.session_scope(), window, reference)
            .or_else(|| window.map(|window| format!("window {window}")))
            .or_else(|| reference.map(str::to_string))
            .or_else(|| pid.map(|pid| format!("pid {pid}")))
            .unwrap_or_else(|| "the desktop".into());
        Some(format!("computer: {action} on {target}"))
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: ActInput = super::parse_input(self.name(), input)?;
        let scope = context.session_scope().to_string();
        let request = match build_request(&input) {
            Ok(request) => request,
            Err(message) => return Ok(ToolOutput::error(message)),
        };
        let activate = input.activate;
        let computer = self.computer.clone();
        match blocking_raw(move || computer.act(&scope, request, activate)).await {
            Ok(outcome) => Ok(ToolOutput::ok(describe_outcome(&outcome))),
            Err(error) => Ok(ToolOutput::error(error.to_string())),
        }
    }
}

fn build_request(input: &ActInput) -> Result<ActRequest, String> {
    let point = match (input.x, input.y) {
        (Some(x), Some(y)) => Some(Point { x, y }),
        (None, None) => None,
        _ => return Err("click needs both `x` and `y`, or neither".into()),
    };
    match input.action {
        ActAction::ActivateApp => {
            let pid = input.pid.ok_or("activate_app needs `pid`")?;
            Ok(ActRequest::ActivateApp { pid })
        }
        ActAction::FocusWindow => {
            let window = input.window.ok_or("focus_window needs `window`")?;
            Ok(ActRequest::FocusWindow { window })
        }
        ActAction::Click => {
            if input.reference.is_some() && point.is_some() {
                return Err("click takes `reference` or `x`/`y`, not both".into());
            }
            if input.reference.is_none() && point.is_none() {
                return Err("click needs `reference` or `x`/`y`".into());
            }
            Ok(ActRequest::Click {
                window: input.window,
                reference: input.reference.clone(),
                point,
                button: match input.button {
                    Button::Left => MouseButton::Left,
                    Button::Right => MouseButton::Right,
                },
                count: input.count.clamp(1, 3),
            })
        }
        ActAction::Type => Ok(ActRequest::Type {
            window: input.window,
            reference: input.reference.clone(),
            text: input
                .text
                .clone()
                .filter(|text| !text.is_empty())
                .ok_or("type needs non-empty `text`")?,
            submit: input.submit,
        }),
        ActAction::Key => Ok(ActRequest::Key {
            window: input.window.ok_or("key needs `window`")?,
            key: input
                .key
                .clone()
                .filter(|key| !key.trim().is_empty())
                .ok_or("key needs `key`")?,
            modifiers: input
                .modifiers
                .iter()
                .copied()
                .map(Modifier::from)
                .collect(),
        }),
        ActAction::Scroll => {
            let dx = input.dx.unwrap_or(0.0);
            let dy = input.dy.unwrap_or(0.0);
            if dx == 0.0 && dy == 0.0 {
                return Err("scroll needs a non-zero `dx` or `dy`".into());
            }
            Ok(ActRequest::Scroll {
                window: input.window.ok_or("scroll needs `window`")?,
                point,
                guard_bounds: None,
                dx,
                dy,
            })
        }
        ActAction::SetValue => Ok(ActRequest::SetValue {
            window: input.window,
            reference: input
                .reference
                .clone()
                .ok_or("set_value needs `reference`")?,
            value: input.value.clone().unwrap_or_default(),
        }),
        ActAction::PerformAction => Ok(ActRequest::PerformAction {
            window: input.window,
            reference: input
                .reference
                .clone()
                .ok_or("perform_action needs `reference`")?,
            action: input
                .accessibility_action
                .clone()
                .filter(|action| !action.trim().is_empty())
                .ok_or("perform_action needs `accessibility_action`")?,
        }),
    }
}

// ── rendering ───────────────────────────────────────────────────────────────

fn describe_availability(availability: &Availability) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "platform: {}", availability.platform);
    let _ = writeln!(text, "supported: {}", availability.supported);
    let _ = writeln!(
        text,
        "interactive_session: {}",
        availability.interactive_session
    );
    let _ = writeln!(
        text,
        "accessibility: {}",
        permission(availability.accessibility)
    );
    let _ = writeln!(
        text,
        "screen_recording: {}",
        permission(availability.screen_recording)
    );
    for note in &availability.notes {
        let _ = writeln!(text, "note: {note}");
    }
    text
}

fn permission(value: leveler_cua::PermissionState) -> &'static str {
    match value {
        leveler_cua::PermissionState::Granted => "granted",
        leveler_cua::PermissionState::Denied => "denied",
        leveler_cua::PermissionState::Unsupported => "unsupported",
        leveler_cua::PermissionState::Unknown => "unknown",
    }
}

fn describe_windows(windows: &[WindowInfo], filter: Option<&str>) -> String {
    if windows.is_empty() {
        return match filter {
            Some(filter) => format!("no windows match {filter:?}"),
            None => "no windows found".to_string(),
        };
    }
    let mut text = format!("{} windows\n", windows.len());
    for window in windows {
        let _ = writeln!(
            text,
            "id={} pid={} app={:?} title={:?} bounds={} on_screen={}{}",
            window.id,
            window.pid,
            window.app,
            window.title,
            describe_bounds(window),
            window.on_screen,
            if window.focused { " focused" } else { "" }
        );
    }
    text
}

fn describe_bounds(window: &WindowInfo) -> String {
    format!(
        "{:.0},{:.0} {:.0}x{:.0}",
        window.bounds.x, window.bounds.y, window.bounds.width, window.bounds.height
    )
}

fn describe_tree(observation: &Observation) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "window id={} app={:?} title={:?} bounds={} on_screen={}",
        observation.window.id,
        observation.window.app,
        observation.window.title,
        describe_bounds(&observation.window),
        observation.window.on_screen
    );
    let _ = writeln!(
        text,
        "nodes: {} truncated: {}",
        observation.node_count, observation.truncated
    );
    let _ = writeln!(
        text,
        "references are valid until this window is observed again"
    );
    render_element(&mut text, &observation.root, 0);
    text
}

fn render_element(text: &mut String, element: &ElementInfo, depth: usize) {
    let _ = write!(text, "{}{}", "  ".repeat(depth), element.role);
    if let Some(title) = element.title.as_deref().filter(|title| !title.is_empty()) {
        let _ = write!(text, " {title:?}");
    } else if let Some(description) = element
        .description
        .as_deref()
        .filter(|description| !description.is_empty())
    {
        let _ = write!(text, " {description:?}");
    }
    let _ = write!(text, " ref={}", element.reference);
    if let Some(value) = element.value.as_deref().filter(|value| !value.is_empty()) {
        let _ = write!(text, " value={value:?}");
    }
    if let Some(identifier) = element
        .identifier
        .as_deref()
        .filter(|identifier| !identifier.is_empty())
    {
        let _ = write!(text, " id={identifier:?}");
    }
    if !element.actions.is_empty() {
        let _ = write!(text, " actions=[{}]", element.actions.join(","));
    }
    if let Some(bounds) = element.bounds {
        let _ = write!(
            text,
            " bounds={:.0},{:.0} {:.0}x{:.0}",
            bounds.x, bounds.y, bounds.width, bounds.height
        );
    }
    if element.enabled == Some(false) {
        let _ = write!(text, " disabled");
    }
    if element.focused == Some(true) {
        let _ = write!(text, " focused");
    }
    text.push('\n');
    for child in &element.children {
        render_element(text, child, depth + 1);
    }
}

fn describe_outcome(outcome: &ActOutcome) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "action: {}", outcome.action);
    let _ = writeln!(text, "target: {}", outcome.target);
    let _ = writeln!(text, "status: {}", status(outcome.status));
    let _ = writeln!(text, "detail: {}", outcome.detail);
    if let Some(window) = &outcome.window {
        let _ = writeln!(
            text,
            "window: id={} title={:?} bounds={} on_screen={}{}",
            window.id,
            window.title,
            describe_bounds(window),
            window.on_screen,
            if window.focused { " focused" } else { "" }
        );
    } else {
        let _ = writeln!(text, "window: closed or no longer present");
    }
    if let Some(app) = &outcome.frontmost_app {
        let _ = writeln!(text, "frontmost: {:?} pid={}", app.name, app.pid);
    }
    text
}

fn status(value: ActStatus) -> &'static str {
    match value {
        ActStatus::Applied => "applied (verified by a fresh observation)",
        ActStatus::CalledOnly => "called_only (the platform accepted it; no effect was verified)",
        ActStatus::Unchanged => "unchanged (the observed state did not change)",
        ActStatus::Unknown => {
            "unknown (the platform accepted it; the result could not be observed)"
        }
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

async fn blocking_raw<T, F>(operation: F) -> leveler_cua::CuaResult<T>
where
    F: FnOnce() -> leveler_cua::CuaResult<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(result) => result,
        Err(error) => Err(leveler_cua::CuaError::Backend {
            backend: "tool",
            detail: format!("desktop worker join failed: {error}"),
        }),
    }
}

fn spawn_cancel_bridge(
    cancellation: &CancellationToken,
    cancel: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let cancellation = cancellation.clone();
    tokio::spawn(async move {
        cancellation.cancelled().await;
        cancel.store(true, Ordering::Relaxed);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: serde_json::Value) -> ActInput {
        serde_json::from_value(value).expect("valid action input")
    }

    #[test]
    fn desktop_observation_is_safe_and_control_is_destructive() {
        let computer = Arc::new(ComputerUse::new());
        assert_eq!(
            ComputerObserveTool::new(computer.clone()).risk(),
            RiskLevel::Safe
        );
        assert_eq!(
            ComputerActTool::new(computer).risk(),
            RiskLevel::Destructive
        );
    }

    #[test]
    fn observation_stays_in_the_canonical_observe_class_but_control_does_not() {
        assert!(crate::registry::OBSERVE_CLASS_TOOLS.contains(&"computer_observe"));
        assert!(!crate::registry::OBSERVE_CLASS_TOOLS.contains(&"computer_act"));
    }

    #[test]
    fn an_approval_prompt_names_the_action_and_target_but_never_the_text() {
        let tool = ComputerActTool::new(Arc::new(ComputerUse::new()));
        let (context, _dir) =
            crate::tools::test_ctx(leveler_execution::PermissionProfile::Assisted, &[]);
        let reason = tool
            .approval_reason(
                &serde_json::json!({
                    "action": "type",
                    "window": 42,
                    "text": "correct horse battery staple",
                }),
                &context,
            )
            .expect("control actions always explain themselves");
        assert!(reason.contains("type"), "{reason}");
        assert!(reason.contains("window 42"), "{reason}");
        assert!(
            !reason.contains("correct"),
            "the prompt leaked typed text: {reason}"
        );
    }

    #[test]
    fn a_click_needs_a_reference_or_a_complete_point_and_not_both() {
        let reference = parse(serde_json::json!({
            "action": "click",
            "reference": "e1-2",
        }));
        assert!(matches!(
            build_request(&reference),
            Ok(ActRequest::Click {
                reference: Some(_),
                point: None,
                ..
            })
        ));

        let point = parse(serde_json::json!({
            "action": "click",
            "window": 7,
            "x": 10.0,
            "y": 20.0,
        }));
        assert!(matches!(
            build_request(&point),
            Ok(ActRequest::Click {
                reference: None,
                point: Some(Point { x: 10.0, y: 20.0 }),
                ..
            })
        ));

        let half = parse(serde_json::json!({
            "action": "click",
            "window": 7,
            "x": 10.0,
        }));
        assert!(build_request(&half).is_err());

        let both = parse(serde_json::json!({
            "action": "click",
            "window": 7,
            "reference": "e1-2",
            "x": 10.0,
            "y": 20.0,
        }));
        assert!(build_request(&both).is_err());
    }

    #[test]
    fn keyboard_and_pointer_actions_require_their_own_fields() {
        let key = parse(serde_json::json!({"action": "key", "window": 7}));
        assert!(build_request(&key).is_err());
        let key = parse(serde_json::json!({
            "action": "key",
            "window": 7,
            "key": "return",
            "modifiers": ["command"],
        }));
        assert!(matches!(
            build_request(&key),
            Ok(ActRequest::Key {
                key,
                modifiers,
                ..
            }) if key == "return" && modifiers == vec![Modifier::Command]
        ));

        let still = parse(serde_json::json!({"action": "scroll", "window": 7}));
        assert!(build_request(&still).is_err());
        let scroll = parse(serde_json::json!({
            "action": "scroll",
            "window": 7,
            "dy": -3.0,
        }));
        assert!(matches!(
            build_request(&scroll),
            Ok(ActRequest::Scroll { dy: -3.0, .. })
        ));
    }

    #[test]
    fn semantic_actions_name_the_element_and_the_value() {
        let set = parse(serde_json::json!({
            "action": "set_value",
            "reference": "e1-2",
            "value": "",
        }));
        assert!(matches!(
            build_request(&set),
            Ok(ActRequest::SetValue { value, .. }) if value.is_empty()
        ));

        let perform = parse(serde_json::json!({
            "action": "perform_action",
            "reference": "e1-2",
        }));
        assert!(build_request(&perform).is_err());
        let perform = parse(serde_json::json!({
            "action": "perform_action",
            "reference": "e1-2",
            "accessibility_action": "AXPress",
        }));
        assert!(matches!(
            build_request(&perform),
            Ok(ActRequest::PerformAction { action, .. }) if action == "AXPress"
        ));
    }
}
