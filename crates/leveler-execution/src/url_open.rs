//! Open user-facing HTTP URLs through the host operating system.

use std::process::ExitStatus;

#[cfg(not(target_os = "windows"))]
use std::process::Command;

/// Failure to hand an HTTP URL to the host's default browser.
#[derive(Debug, thiserror::Error)]
pub enum UrlOpenError {
    #[error("invalid URL `{url}`: {source}")]
    InvalidUrl {
        url: String,
        #[source]
        source: url::ParseError,
    },
    #[error("URL scheme `{scheme}` is not allowed; expected http or https")]
    UnsupportedScheme { scheme: String },
    #[error("URL contains an interior NUL byte")]
    InteriorNul,
    #[error("failed to start `{program}`: {source}")]
    Start {
        program: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("`{program}` failed with status {status}")]
    Exit {
        program: &'static str,
        status: ExitStatus,
    },
    #[cfg(target_os = "windows")]
    #[error("ShellExecuteW rejected the URL with code {code}")]
    WindowsShellExecute { code: isize },
}

/// Ask the host to open `url` in the user's default browser.
///
/// Process-based adapters wait for the platform opener to exit; Windows checks
/// the synchronous `ShellExecuteW` result. `Ok(())` records that the operating
/// system accepted the request, not that a page finished loading. Browser
/// automation remains a separate capability.
pub fn open_url(url: &str) -> Result<(), UrlOpenError> {
    platform_open(validated_url_argument(url)?)
}

/// Validate the URL while preserving the exact text supplied by the caller.
///
/// `url::Url` is deliberately discarded after validation. Passing its
/// normalized representation onward would change case, escapes, or dot
/// segments before the system default-browser handler sees the request.
fn validated_url_argument(value: &str) -> Result<&str, UrlOpenError> {
    if value.contains('\0') {
        return Err(UrlOpenError::InteriorNul);
    }
    let parsed = url::Url::parse(value).map_err(|source| UrlOpenError::InvalidUrl {
        url: value.to_string(),
        source,
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(UrlOpenError::UnsupportedScheme {
            scheme: parsed.scheme().to_string(),
        });
    }
    Ok(value)
}

#[cfg(not(target_os = "windows"))]
fn platform_open(url: &str) -> Result<(), UrlOpenError> {
    let (program, mut command) = platform_command(url);
    let status = command
        .status()
        .map_err(|source| UrlOpenError::Start { program, source })?;
    if !status.success() {
        return Err(UrlOpenError::Exit { program, status });
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_open(url: &str) -> Result<(), UrlOpenError> {
    let result = shell_execute_url(url);
    if !shell_execute_succeeded(result) {
        return Err(UrlOpenError::WindowsShellExecute { code: result });
    }
    Ok(())
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn shell_execute_url(url: &str) -> isize {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let wide = nul_terminated_utf16(url);
    // SAFETY: `wide` is constructed here as a live, NUL-terminated UTF-16
    // buffer and remains alive for the call. The other pointer parameters are
    // null as documented, the window handle is null, and ShellExecuteW does
    // not retain these pointers.
    (unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    }) as isize
}

#[cfg(any(target_os = "windows", test))]
fn nul_terminated_utf16(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(any(target_os = "windows", test))]
fn shell_execute_succeeded(result: isize) -> bool {
    result > 32
}

#[cfg(target_os = "macos")]
fn platform_command(url: &str) -> (&'static str, Command) {
    const PROGRAM: &str = "/usr/bin/open";
    let mut command = Command::new(PROGRAM);
    command.arg(url);
    (PROGRAM, command)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_command(url: &str) -> (&'static str, Command) {
    const PROGRAM: &str = "xdg-open";
    let mut command = Command::new(PROGRAM);
    command.arg(url);
    (PROGRAM, command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_absolute_http_urls() {
        assert!(validated_url_argument("http://127.0.0.1:8080/?token=abc").is_ok());
        assert!(validated_url_argument("https://example.com/path?q=1").is_ok());
        assert!(matches!(
            validated_url_argument("file:///tmp/index.html"),
            Err(UrlOpenError::UnsupportedScheme { scheme }) if scheme == "file"
        ));
        assert!(matches!(
            validated_url_argument("example.com"),
            Err(UrlOpenError::InvalidUrl { .. })
        ));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn platform_adapter_keeps_query_text_in_one_argument() {
        use std::ffi::OsStr;

        let url = "https://example.com/search?a=one&b=two%20words";
        let (_, command) = platform_command(url);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new(url)]
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn open_path_preserves_the_callers_exact_url_text() {
        use std::ffi::OsStr;

        let original = "HtTpS://Example.COM/a/../b?escaped=%2f&letter=%41";
        let validated = validated_url_argument(original).unwrap();
        let (_, command) = platform_command(validated);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new(original)]
        );
    }

    #[test]
    fn windows_uri_buffer_preserves_text_and_has_one_terminator() {
        let url = "HtTpS://Example.COM/a/../b?one=1&two=%2f";
        let wide = nul_terminated_utf16(validated_url_argument(url).unwrap());
        assert_eq!(
            &wide[..wide.len() - 1],
            url.encode_utf16().collect::<Vec<_>>()
        );
        assert_eq!(wide.last(), Some(&0));
        assert!(!wide[..wide.len() - 1].contains(&0));
    }

    #[test]
    fn windows_shell_execute_result_boundary_is_strictly_above_32() {
        assert!(!shell_execute_succeeded(0));
        assert!(!shell_execute_succeeded(32));
        assert!(shell_execute_succeeded(33));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_the_absolute_launch_services_adapter() {
        use std::ffi::OsStr;

        let (program, command) = platform_command("https://example.com/");
        assert_eq!(program, "/usr/bin/open");
        assert_eq!(command.get_program(), OsStr::new("/usr/bin/open"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("https://example.com/")]
        );
    }
}
