//! Decode child-process output bytes without losing the Windows console
//! encoding.
//!
//! Modern tools write UTF-8, but `cmd.exe` and legacy console programs on a
//! Chinese Windows host write the active console code page (GBK/936 by
//! default). Decoding those bytes as UTF-8 turns every Chinese character into
//! `U+FFFD` (`◆`). The decoder keeps valid UTF-8 untouched and uses the
//! console/OEM code page only for the invalid remainder.

/// Decode one standalone stdout/stderr buffer.
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

/// Incremental decoder for one child output stream.
///
/// A pipe read boundary can land inside a multi-byte character. Decoding each
/// read on its own turns the halves into replacement glyphs (UTF-8) or a
/// spurious legacy character, so the live view disagrees with the final
/// capture. The decoder holds an incomplete trailing sequence until the next
/// read; callers flush it once at EOF with [`OutputDecoder::finish`].
///
/// The encoding decision is deliberately per buffer, never per stream: a host
/// whose console code page is 936 still runs tools that emit UTF-8, so valid
/// UTF-8 must win before the legacy fallback.
#[derive(Debug)]
pub struct OutputDecoder {
    pending: Vec<u8>,
    /// Resolved once. The console code page belongs to the host, not to a
    /// read boundary, and re-reading it per chunk could mix two encodings in
    /// one stream.
    #[cfg(windows)]
    code_page: u32,
}

impl Default for OutputDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputDecoder {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            #[cfg(windows)]
            code_page: active_code_page(),
        }
    }

    /// A decoder pinned to an explicit code page, for tests that must not
    /// depend on the host's console configuration.
    #[cfg(all(windows, test))]
    fn with_code_page(code_page: u32) -> Self {
        Self {
            pending: Vec::new(),
            code_page,
        }
    }

    /// Bytes held back for the next read. The read path uses this to account
    /// for a live chunk it could not deliver.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Decode one read, holding an incomplete trailing multi-byte sequence.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return String::new();
        }
        let mut combined = std::mem::take(&mut self.pending);
        combined.extend_from_slice(bytes);
        match std::str::from_utf8(&combined) {
            Ok(text) => return text.to_owned(),
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                // An incomplete UTF-8 sequence is at most three bytes, and
                // everything before it is valid by definition.
                if combined.len() - valid <= 3 {
                    self.pending.extend_from_slice(&combined[valid..]);
                    return String::from_utf8_lossy(&combined[..valid]).into_owned();
                }
            }
            Err(_) => {}
        }
        #[cfg(windows)]
        if let Some(text) = self.hold_legacy_tail(&combined) {
            return text;
        }
        self.decode_legacy(&combined)
    }

    /// Resolve bytes still held at EOF. An incomplete sequence has no more
    /// bytes coming, so the lossy fallback is the honest final answer.
    pub fn finish(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let bytes = std::mem::take(&mut self.pending);
        self.decode_legacy(&bytes)
    }

    #[cfg(not(windows))]
    fn decode_legacy(&self, bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[cfg(windows)]
    fn decode_legacy(&self, bytes: &[u8]) -> String {
        if self.code_page == 65001 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        decode_with_code_page(bytes, self.code_page)
            .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
    }

    /// Hold a trailing DBCS lead byte that has no following byte yet.
    ///
    /// Only a byte that is a lead byte for the active code page is eligible,
    /// and only when everything before it decodes cleanly. Anything else is a
    /// real malformed byte, not an incomplete character, and is emitted now
    /// rather than delayed to the next read.
    #[cfg(windows)]
    fn hold_legacy_tail(&mut self, combined: &[u8]) -> Option<String> {
        if self.code_page == 65001 {
            return None;
        }
        let last = *combined.last()?;
        if !is_dbcs_lead_byte(self.code_page, last) {
            return None;
        }
        let prefix = &combined[..combined.len() - 1];
        let text = decode_with_code_page_flags(prefix, self.code_page, MB_ERR_INVALID_CHARS)?;
        self.pending.push(last);
        Some(text)
    }
}

#[cfg(not(windows))]
fn decode_platform(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The code page legacy console output should be decoded with.
///
/// A zero result means "no console"; the OEM code page is the system-wide
/// fallback for console programs writing to a pipe. A UTF-8 console still
/// receives legacy bytes from tools that ignore the console code page, so the
/// OEM page is the best available recovery there too.
#[cfg(windows)]
#[allow(unsafe_code)]
fn active_code_page() -> u32 {
    use windows_sys::Win32::Globalization::GetOEMCP;
    use windows_sys::Win32::System::Console::GetConsoleOutputCP;

    // SAFETY: both calls take no pointers and only read the process's console
    // configuration.
    let console = unsafe { GetConsoleOutputCP() };
    if console == 0 || console == 65001 {
        let oem = unsafe { GetOEMCP() };
        if oem == 0 { 65001 } else { oem }
    } else {
        console
    }
}

#[cfg(windows)]
fn decode_platform(bytes: &[u8]) -> String {
    let code_page = active_code_page();
    if code_page == 65001 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    decode_with_code_page(bytes, code_page)
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).into_owned())
}

/// `MultiByteToWideChar` treats malformed bytes as an error instead of
/// silently replacing them.
#[cfg(windows)]
const MB_ERR_INVALID_CHARS: u32 = 0x0000_0008;

/// Whether `byte` can start a two-byte character in `code_page`.
///
/// Only the DBCS code pages are listed. A single-byte or unknown code page has
/// no incomplete trailing character, so nothing may be deferred for it.
#[cfg(windows)]
fn is_dbcs_lead_byte(code_page: u32, byte: u8) -> bool {
    match code_page {
        // Shift-JIS.
        932 => (0x81..=0x9F).contains(&byte) || (0xE0..=0xFC).contains(&byte),
        // GBK, UHC, Big5.
        936 | 949 | 950 => (0x81..=0xFE).contains(&byte),
        // Johab.
        1361 => {
            (0x84..=0xD3).contains(&byte)
                || (0xD8..=0xDE).contains(&byte)
                || (0xE0..=0xF9).contains(&byte)
        }
        _ => false,
    }
}

/// Decode `bytes` with an explicit multibyte code page.
///
/// The first `MultiByteToWideChar` call measures the UTF-16 length; the second
/// fills a buffer of that exact length. Both calls receive the same valid
/// input slice and a null output pointer for the measurement pass, so the
/// output capacity is never guessed.
#[cfg(windows)]
fn decode_with_code_page(bytes: &[u8], code_page: u32) -> Option<String> {
    decode_with_code_page_flags(bytes, code_page, 0)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn decode_with_code_page_flags(bytes: &[u8], code_page: u32, flags: u32) -> Option<String> {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;

    if bytes.is_empty() {
        return Some(String::new());
    }
    let len = i32::try_from(bytes.len()).ok()?;
    // SAFETY: `bytes` is valid for `len` reads; the measurement call passes a
    // null output pointer and zero capacity, as the API requires.
    let wide_len = unsafe {
        MultiByteToWideChar(
            code_page,
            flags,
            bytes.as_ptr(),
            len,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return None;
    }
    let mut wide = vec![0u16; wide_len as usize];
    // SAFETY: `wide` is allocated with exactly `wide_len` elements, and the
    // API was just asked for that many UTF-16 units for this same input.
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            flags,
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

    #[test]
    fn a_utf8_character_split_across_reads_reassembles() {
        let bytes = "中文".as_bytes();
        let mut decoder = OutputDecoder::new();
        assert_eq!(decoder.push(&bytes[..1]), "");
        assert_eq!(decoder.push(&bytes[1..4]), "中");
        assert_eq!(decoder.push(&bytes[4..]), "文");
        assert_eq!(decoder.finish(), "");
    }

    #[cfg(windows)]
    #[test]
    fn gbk_bytes_decode_to_chinese() {
        // "中文" in code page 936.
        let bytes = [0xD6, 0xD0, 0xCE, 0xC4];
        assert_eq!(decode_with_code_page(&bytes, 936).as_deref(), Some("中文"));
    }

    #[cfg(windows)]
    #[test]
    fn a_gbk_lead_byte_is_held_until_its_trail_arrives() {
        let bytes = [0xD6, 0xD0, 0xCE, 0xC4];
        let mut decoder = OutputDecoder::with_code_page(936);
        assert_eq!(decoder.push(&bytes[..1]), "");
        assert_eq!(decoder.pending_len(), 1);
        assert_eq!(decoder.push(&bytes[1..3]), "中");
        assert_eq!(decoder.pending_len(), 1);
        assert_eq!(decoder.push(&bytes[3..]), "文");
        assert_eq!(decoder.finish(), "");
    }

    #[cfg(windows)]
    #[test]
    fn an_incomplete_gbk_character_is_resolved_at_finish() {
        let mut decoder = OutputDecoder::with_code_page(936);
        assert_eq!(decoder.push(&[0xD6]), "");
        let resolved = decoder.finish();
        assert!(
            !resolved.is_empty(),
            "the partial byte has an honest result"
        );
        assert_eq!(decoder.finish(), "");
    }

    #[cfg(windows)]
    #[test]
    fn a_malformed_gbk_byte_is_not_deferred_as_a_partial_character() {
        let mut decoder = OutputDecoder::with_code_page(936);
        // 0xFF cannot lead a GBK character.
        let first = decoder.push(&[0xFF]);
        assert!(!first.is_empty(), "the malformed byte is resolved at once");
        assert_eq!(decoder.pending_len(), 0);
        assert_eq!(decoder.push(b"ok"), "ok");
    }

    #[cfg(not(windows))]
    #[test]
    fn invalid_bytes_remain_lossy() {
        assert_eq!(decode_output(&[0xFF]), "\u{FFFD}");
    }

    #[cfg(not(windows))]
    #[test]
    fn an_incomplete_sequence_is_resolved_at_finish() {
        let mut decoder = OutputDecoder::new();
        assert_eq!(decoder.push(&[0xE4]), "");
        assert_eq!(decoder.finish(), "\u{FFFD}");
    }
}
