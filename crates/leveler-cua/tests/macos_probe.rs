//! A real-desktop probe. It is ignored by default because it needs an
//! interactive session and TCC grants; run it explicitly:
//!
//! ```text
//! cargo test -p leveler-cua --test macos_probe -- --ignored --nocapture
//! ```

#![cfg(target_os = "macos")]

use leveler_cua::ComputerUse;

#[test]
#[ignore = "requires an interactive macOS session with Accessibility and Screen Recording"]
fn observe_the_live_desktop() {
    let computer = ComputerUse::new();
    let availability = computer.availability().expect("availability");
    println!("availability: {availability:?}");
    assert!(availability.supported);

    let apps = computer.apps().expect("apps");
    println!("apps: {}", apps.len());
    for app in apps.iter().take(10) {
        println!(
            "  pid={} name={:?} windows={} frontmost={}",
            app.pid, app.name, app.window_count, app.frontmost
        );
    }

    let windows = computer.windows("probe", None).expect("windows");
    println!("windows: {}", windows.len());
    for window in windows.iter().take(10) {
        println!(
            "  id={} pid={} app={:?} title={:?} bounds={:?} on_screen={} focused={}",
            window.id,
            window.pid,
            window.app,
            window.title,
            window.bounds,
            window.on_screen,
            window.focused
        );
    }

    let Some(window) = windows
        .iter()
        .find(|window| !window.title.is_empty() && window.on_screen)
    else {
        println!("no titled on-screen window; skipping tree and capture");
        return;
    };
    println!("observing window {} {:?}", window.id, window.title);
    match computer.observe_window("probe", window.id, 4, 200, None) {
        Ok(observation) => {
            println!(
                "tree: role={:?} title={:?} nodes={} truncated={}",
                observation.root.role,
                observation.root.title,
                observation.node_count,
                observation.truncated
            );
            fn print_children(node: &leveler_cua::ElementInfo, depth: usize) {
                for child in &node.children {
                    println!(
                        "{}{} {:?} ref={} actions={:?}",
                        "  ".repeat(depth),
                        child.role,
                        child.title.as_deref().or(child.description.as_deref()),
                        child.reference,
                        child.actions
                    );
                    if depth < 2 {
                        print_children(child, depth + 1);
                    }
                }
            }
            print_children(&observation.root, 0);
        }
        Err(error) => println!("tree failed: {error}"),
    }

    match computer.screenshot(Some(window.id)) {
        Ok(screenshot) => {
            println!(
                "screenshot: {}x{} scale={} target={:?} bytes={}",
                screenshot.width,
                screenshot.height,
                screenshot.scale,
                screenshot.target,
                screenshot.png.len()
            );
            let path = std::env::temp_dir().join("leveler-cua-probe.png");
            std::fs::write(&path, &screenshot.png).expect("write probe png");
            println!("wrote {}", path.display());
        }
        Err(error) => println!("screenshot failed: {error}"),
    }
}
