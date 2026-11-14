//! Decode child-process output bytes without losing the Windows console
//! encoding.
//!
//! Modern tools write UTF-8, but `cmd.exe` and legacy console programs on a
//! Chinese Windows host write the active console code page (GBK/936 by
//! default). Decoding those bytes as UTF-8 turns every Chinese character into
//! `U+FFFD` (`◆`). The decoder keeps valid UTF-8 untouched and uses the
//! console/OEM code page only for the invalid remainder.

/// Decode one stdout/stderr chunk.
///
/// Valid UTF-8 is returned verbatim. On Windows, invalid UTF-8 is decoded with
/// the active console output code page, falling back to the OEM code page when
/// the console itself is UTF-8 or has no console. Other hosts keep the
/// lossy-UTF-8 behavior.
pub fn decode_output(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => decode_platform(bytes),
    }
}

#[cfg(not(windows))]
fn decode_platform(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn decode_platform(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::GetOEMCP;
    use windows_sys::Win32::System::Console::GetConsoleOutputCP;

    // SAFETY: both calls take no pointers and only read the process's console
    // configuration. A zero result means "no console"; the OEM code page is
    // the system-wide fallback for console programs writing to a pipe.
    let console = unsafe { GetConsoleOutputCP() };
    // A UTF-8 console still receives legacy bytes from tools that ignore the
    // console code page; the OEM page is the best available recovery.
    let code_page = if console == 0 || console == 65001 {
        let oem = unsafe { GetOEMCP() };
        if oem == 0 { 65001 } else { oem }
    } else {
        console
    };
    if code_page == 65001 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    decode_with_code_page(bytes, code_page)
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

/// Decode `bytes` with an explicit multibyte code page.
///
/// The first `MultiByteToWideChar` call measures the UTF-16 length; the second
/// fills a buffer of that exact length. Both calls receive the same valid
/// input slice and a null output pointer for the measurement pass, so the
/// output capacity is never guessed.
#[cfg(windows)]
#[allow(unsafe_code)]
fn decode_with_code_page(bytes: &[u8], code_page: u32) -> Option<String> {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;

    if bytes.is_empty() {
        return Some(String::new());
    }
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: `bytes` is valid for `len` reads; the measurement call passes a
    // null output pointer and zero capacity, as the API requires.
    let wide_len =
        unsafe { MultiByteToWideChar(code_page, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
    if wide_len <= 0 {
        return None;
    }
    let mut wide = vec![0u16; wide_len as usize];
    // SAFETY: `wide` is allocated with exactly `wide_len` elements, and the
    // API was just asked for that many UTF-16 units for this same input.
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            len,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    if written <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&wide[..written as usize]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_utf8_is_unchanged() {
        assert_eq!(decode_output("中文 English".as_bytes()), "中文 English");
    }

    #[cfg(windows)]
    #[test]
    fn gbk_bytes_decode_to_chinese() {
        // "中文" in code page 936.
        let bytes = [0xD6, 0xD0, 0xCE, 0xC4];
        assert_eq!(decode_with_code_page(&bytes, 936).as_deref(), Some("中文"));
    }

    #[cfg(not(windows))]
    #[test]
    fn invalid_bytes_remain_lossy() {
        assert_eq!(decode_output(&[0xFF]), "\u{FFFD}");
    }
}
