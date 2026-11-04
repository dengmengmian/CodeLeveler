//! Window / display screenshots.
//!
//! `CGDisplayCreateImage` and `CGWindowListCreateImage` are obsoleted on
//! macOS 15 and fail to build against current SDKs. The system capture tool
//! remains the supported process-level path, so the backend spawns it with a
//! private temporary directory and reads the PNG back. No image is logged.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::{CuaError, CuaResult, PermissionKind};
use crate::types::{Screenshot, WindowInfo};

use super::cf::{
    CGDisplayBounds, CGDisplayPixelsWide, CGMainDisplayID, CGPreflightScreenCaptureAccess,
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) fn capture(window: Option<&WindowInfo>) -> CuaResult<Screenshot> {
    if unsafe { CGPreflightScreenCaptureAccess() } == 0 {
        return Err(CuaError::PermissionDenied {
            kind: PermissionKind::ScreenRecording,
            detail: "Screen Recording is not granted to the process that launched CodeLeveler; \
                     System Settings → Privacy & Security → Screen & System Audio Recording"
                .into(),
        });
    }

    let directory = tempfile::Builder::new()
        .prefix("leveler-cua-")
        .tempdir()
        .map_err(|error| {
            backend(format!(
                "could not create a private capture directory: {error}"
            ))
        })?;
    let path = directory.path().join("screen.png");
    let mut command = Command::new("/usr/sbin/screencapture");
    command.arg("-x").arg("-o");
    if let Some(window) = window {
        command.arg(format!("-l{}", window.id));
    }
    command.arg(&path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| backend(format!("could not start screencapture: {error}")))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.stderr.take() {
                        use std::io::Read;
                        let _ = pipe.read_to_string(&mut stderr);
                    }
                    return Err(backend(format!(
                        "screencapture exited with {}: {}",
                        status,
                        stderr.trim()
                    )));
                }
                break;
            }
            Ok(None) if started.elapsed() < CAPTURE_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CuaError::Timeout(
                    "screencapture did not finish within 15s".into(),
                ));
            }
            Err(error) => {
                return Err(backend(format!(
                    "could not wait for screencapture: {error}"
                )));
            }
        }
    }

    let png = std::fs::read(&path)
        .map_err(|error| backend(format!("could not read the captured image: {error}")))?;
    let (width, height) =
        png_dimensions(&png).ok_or_else(|| backend("screencapture did not produce a valid PNG"))?;
    let scale = match window {
        Some(window) if window.bounds.width > 0.0 => width as f64 / window.bounds.width,
        Some(_) => 1.0,
        None => {
            let display = unsafe { CGMainDisplayID() };
            let bounds = unsafe { CGDisplayBounds(display) };
            let pixels = unsafe { CGDisplayPixelsWide(display) };
            if bounds.size.width > 0.0 && pixels > 0 {
                pixels as f64 / bounds.size.width
            } else {
                1.0
            }
        }
    };
    let target = match window {
        Some(window) => format!("window {} {:?}", window.id, window.title),
        None => "main display".to_string(),
    };
    Ok(Screenshot {
        png,
        width,
        height,
        scale,
        target,
        window: window.cloned(),
    })
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

fn backend(detail: impl Into<String>) -> CuaError {
    CuaError::Backend {
        backend: "macos",
        detail: detail.into(),
    }
}
