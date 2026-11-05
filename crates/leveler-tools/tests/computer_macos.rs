//! Real macOS acceptance for `computer_observe` and `computer_act`.
//!
//! These are ignored by default: they drive a live desktop and need an
//! interactive session with Accessibility and Screen Recording grants. Run:
//!
//! ```text
//! cargo test -p leveler-tools --test computer_macos -- --ignored --nocapture
//! ```
//!
//! The test uses Calculator because it is stateless, has no document to save,
//! and exposes real AX buttons. It proves, in order: availability reporting,
//! window discovery, an AX tree with usable references, an AXPress action
//! verified by a fresh observation, raw keyboard input after focus, and a
//! screenshot whose reported scale matches the captured image.

#![cfg(target_os = "macos")]

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use leveler_cua::ComputerUse;
use leveler_execution::Workspace;
use leveler_tools::tools::{ComputerActTool, ComputerObserveTool};
use leveler_tools::{PermissionProfile, Tool, ToolContext};
use tokio_util::sync::CancellationToken;

const APP: &str = "Calculator";

struct Harness {
    observe: ComputerObserveTool,
    act: ComputerActTool,
    context: ToolContext,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("a temporary workspace");
        let context = ToolContext::new(
            Workspace::new(dir.path()).expect("a workspace"),
            PermissionProfile::FullAccess,
        );
        let computer = Arc::new(ComputerUse::new());
        Self {
            observe: ComputerObserveTool::new(computer.clone()),
            act: ComputerActTool::new(computer),
            context,
            _dir: dir,
        }
    }

    async fn observe_output(&self, input: serde_json::Value) -> leveler_tools::ToolOutput {
        self.observe
            .execute(input, self.context.clone(), CancellationToken::new())
            .await
            .expect("the observe tool runs")
    }

    async fn observe(&self, input: serde_json::Value) -> String {
        let output = self.observe_output(input).await;
        assert!(
            !output.is_error,
            "computer_observe reported: {}",
            output.content
        );
        output.content
    }

    async fn act(&self, input: serde_json::Value) -> String {
        let output = self
            .act
            .execute(input, self.context.clone(), CancellationToken::new())
            .await
            .expect("the act tool runs");
        assert!(
            !output.is_error,
            "computer_act reported: {}",
            output.content
        );
        output.content
    }

    async fn windows(&self, filter: Option<&str>) -> Vec<String> {
        let mut input = serde_json::json!({"action": "windows"});
        if let Some(filter) = filter {
            input["app"] = serde_json::Value::String(filter.to_string());
        }
        self.observe(input)
            .await
            .lines()
            .filter(|line| line.starts_with("id="))
            .map(str::to_string)
            .collect()
    }

    async fn tree(&self, window: u64) -> String {
        self.observe(serde_json::json!({
            "action": "tree",
            "window": window,
            "depth": 6,
            "max_nodes": 500,
        }))
        .await
    }
}

fn window_id(line: &str) -> u64 {
    line.split_whitespace()
        .find_map(|token| token.strip_prefix("id="))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("no window id in {line:?}"))
}

fn refs<'a>(tree: &'a str, needle: &str) -> Vec<&'a str> {
    tree.lines()
        .filter(|line| line.contains(needle))
        .filter_map(|line| {
            line.split_whitespace()
                .find(|token| token.starts_with("ref="))
        })
        .collect()
}

fn find_button(tree: &str, title: &str) -> String {
    let needle = format!("AXButton {title:?}");
    let found = refs(tree, &needle);
    assert!(
        !found.is_empty(),
        "no button {title:?} in the tree:\n{tree}"
    );
    found[0].trim_start_matches("ref=").to_string()
}

fn find_button_by_id(tree: &str, id: &str) -> Option<String> {
    let token = format!("id=\"{id}\"");
    let line = tree
        .lines()
        .find(|line| line.contains("AXButton") && line.contains(&token))?;
    let reference = line
        .split_whitespace()
        .find(|token| token.starts_with("ref="))?
        .trim_start_matches("ref=");
    Some(reference.to_string())
}

/// The Calculator input display is the static text under `StandardInputView`.
/// Reading it back is how an action is verified: the tool's own success wording
/// is not evidence.
fn input_display(tree: &str) -> Option<String> {
    let lines: Vec<&str> = tree.lines().collect();
    let start = lines.iter().position(|line| {
        line.contains("AXScrollArea") && line.contains("id=\"StandardInputView\"")
    })?;
    // The accessibility action names in the parent line contain newlines, so
    // the child is the next real element line, not the next physical line.
    let value = lines[start + 1..]
        .iter()
        .find(|line| line.trim_start().starts_with("AXStaticText") && line.contains("value=\""))?;
    let raw = value.split("value=\"").nth(1)?.split('"').next()?;
    // The display prefixes digits with U+200E; Rust renders it escaped.
    Some(raw.replace("\\u{200e}", ""))
}

fn calculator_pid() -> Option<u32> {
    let output = Command::new("pgrep").args(["-x", APP]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .next_back()
}

fn start_calculator() -> (u32, bool) {
    if let Some(pid) = calculator_pid() {
        return (pid, false);
    }
    let status = Command::new("open")
        .args(["-a", APP])
        .status()
        .expect("open Calculator");
    assert!(status.success(), "open -a {APP} failed");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = calculator_pid() {
            return (pid, true);
        }
        assert!(Instant::now() < deadline, "{APP} never started");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop_calculator() {
    let _ = Command::new("pkill").args(["-x", APP]).status();
}

fn frontmost_pid(apps: &str) -> Option<u32> {
    apps.lines()
        .find(|line| line.ends_with(" frontmost"))
        .and_then(|line| {
            line.split_whitespace()
                .find_map(|token| token.strip_prefix("pid="))
                .and_then(|value| value.parse().ok())
        })
}

async fn wait_for_display(harness: &Harness, window: u64, expected: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let tree = harness.tree(window).await;
        let display = input_display(&tree).unwrap_or_default();
        if display == expected || Instant::now() >= deadline {
            return display;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_window(harness: &Harness, pid: u32) -> String {
    let filter = pid.to_string();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let windows = harness.windows(Some(&filter)).await;
        if let Some(window) = windows.iter().find(|line| line.contains("on_screen=true")) {
            return window.clone();
        }
        assert!(
            Instant::now() < deadline,
            "{APP} (pid {pid}) did not put an on-screen window up; saw {windows:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an interactive macOS session with Accessibility and Screen Recording"]
async fn the_tools_drive_a_real_window_and_verify_every_effect() {
    let harness = Harness::new();
    let (app_pid, started_here) = start_calculator();

    let availability = harness
        .observe(serde_json::json!({"action": "availability"}))
        .await;
    println!("availability:\n{availability}");
    for expected in [
        "platform: macos",
        "supported: true",
        "interactive_session: true",
        "accessibility: granted",
        "screen_recording: granted",
    ] {
        assert!(
            availability.contains(expected),
            "availability lost {expected:?}: {availability}"
        );
    }

    let window = wait_for_window(&harness, app_pid).await;
    let id = window_id(&window);
    println!("target window: {window}");

    let before = harness.observe(serde_json::json!({"action": "apps"})).await;
    let previous_frontmost = frontmost_pid(&before);

    let tree = harness.tree(id).await;
    println!("initial tree:\n{tree}");
    assert!(
        tree.contains("references are valid until this window is observed again"),
        "the tree must state the reference lifetime"
    );

    // 0. Start from a known display: the keypad's own clear button, found by
    // its stable identifier rather than its localized title.
    let clear = find_button_by_id(&tree, "AllClear")
        .or_else(|| find_button_by_id(&tree, "Clear"))
        .unwrap_or_else(|| panic!("no clear button in the tree:\n{tree}"));
    let outcome = harness
        .act(serde_json::json!({"action": "click", "reference": clear}))
        .await;
    println!("clear:\n{outcome}");
    let display = wait_for_display(&harness, id, "0").await;
    assert_eq!(display, "0", "the clear button did not reset the display");

    // 1. A semantic AX action through a reference, verified by re-observing.
    let tree = harness.tree(id).await;
    let seven = find_button(&tree, "7");
    let outcome = harness
        .act(serde_json::json!({"action": "click", "reference": seven}))
        .await;
    println!("click 7:\n{outcome}");
    assert!(outcome.contains("status: called_only"), "{outcome}");

    let display = wait_for_display(&harness, id, "7").await;
    println!("display after 7: {display:?}");
    assert_eq!(display, "7", "the display did not show 7 after AXPress");

    // 2. Raw keyboard input: raise the window, type an 8, then clear with Esc.
    let focused = harness
        .act(serde_json::json!({
            "action": "focus_window",
            "window": id,
            "activate": true,
        }))
        .await;
    println!("focus:\n{focused}");
    assert!(focused.contains("status: applied"), "{focused}");

    let typed = harness
        .act(serde_json::json!({
            "action": "key",
            "window": id,
            "key": "8",
            "activate": true,
        }))
        .await;
    println!("key 8:\n{typed}");
    assert!(typed.contains("status: called_only"), "{typed}");

    let display = wait_for_display(&harness, id, "78").await;
    println!("display after 8: {display:?}");
    assert_eq!(display, "78", "raw key input did not reach the display");

    let cleared = harness
        .act(serde_json::json!({
            "action": "key",
            "window": id,
            "key": "escape",
            "activate": true,
        }))
        .await;
    println!("escape:\n{cleared}");
    let display = wait_for_display(&harness, id, "0").await;
    println!("display after escape: {display:?}");
    assert_eq!(display, "0", "Escape did not clear the display");

    // 3. A screenshot of this window: the reported scale must match the image.
    let output = harness
        .observe_output(serde_json::json!({"action": "screenshot", "window": id}))
        .await;
    assert!(!output.is_error, "screenshot failed: {}", output.content);
    let text = output.content;
    println!("screenshot:\n{text}");
    let media = output.metadata["image"]["media_type"]
        .as_str()
        .expect("the screenshot carries an image part");
    assert_eq!(media, "image/png");
    let data = output.metadata["image"]["data"]
        .as_str()
        .expect("the image part carries data");
    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("valid base64 image data");
    let decoded = image::load_from_memory(&png).expect("a decodable PNG");
    let reported = text
        .lines()
        .find_map(|line| line.strip_prefix("image: "))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|size| size.split_once('x'))
        .expect("the text reports the image dimensions");
    assert_eq!(
        (decoded.width(), decoded.height()),
        (
            reported.0.parse().expect("width"),
            reported.1.parse().expect("height")
        )
    );
    let scale: f64 = text
        .lines()
        .find_map(|line| line.strip_prefix("scale: "))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .expect("the text reports the screenshot scale");
    // The screenshot reports the bounds it captured. The window line from the
    // first discovery can predate a restore animation (226x402 before the
    // 230x408 frame), so projecting a stale width would compare two frames.
    let captured_window = text
        .lines()
        .find_map(|line| line.strip_prefix("window: "))
        .expect("the screenshot reports the captured window");
    let logical_width = captured_window
        .split("bounds=")
        .nth(1)
        .and_then(|rest| rest.split(' ').nth(1))
        .and_then(|size| size.split_once('x'))
        .and_then(|(width, _)| width.parse::<f64>().ok())
        .expect("the window line reports its logical width");
    let projected = logical_width * scale;
    assert!(
        (projected - f64::from(decoded.width())).abs() <= 2.0,
        "scale {scale} projects {logical_width} points to {projected} px, \
         but the image is {} px wide",
        decoded.width()
    );

    // 4. Put the user's frontmost application back.
    if let Some(pid) = previous_frontmost.filter(|pid| *pid != app_pid) {
        let restored = harness
            .act(serde_json::json!({"action": "activate_app", "pid": pid}))
            .await;
        println!("restored frontmost pid {pid}:\n{restored}");
    }

    if started_here {
        stop_calculator();
    }
}
