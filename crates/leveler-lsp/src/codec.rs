//! LSP message framing: `Content-Length: N\r\n\r\n<json>` (JSON-RPC 2.0).

/// Encode a JSON body as an LSP wire message.
pub fn encode(json: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
    out.extend_from_slice(json.as_bytes());
    out
}

/// Incrementally reassembles LSP frames from a byte stream that may split a
/// header or body at any boundary.
#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// Pull the next complete message body (JSON string), if one is buffered.
    pub fn next_message(&mut self) -> Option<String> {
        // Find the header/body separator.
        let sep = find_subsequence(&self.buffer, b"\r\n\r\n")?;
        let header = &self.buffer[..sep];
        let content_length = parse_content_length(header)?;

        let body_start = sep + 4;
        if self.buffer.len() < body_start + content_length {
            return None; // body not fully arrived
        }
        let body = self.buffer[body_start..body_start + content_length].to_vec();
        self.buffer.drain(..body_start + content_length);
        String::from_utf8(body).ok()
    }
}

fn parse_content_length(header: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(header).ok()?;
    for line in text.split("\r\n") {
        if let Some(value) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            return value.trim().parse().ok();
        }
    }
    None
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_single_message() {
        let wire = encode(r#"{"a":1}"#);
        let mut r = FrameReader::new();
        r.feed(&wire);
        assert_eq!(r.next_message().as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(r.next_message(), None);
    }

    #[test]
    fn handles_byte_fragmentation() {
        let wire = encode(r#"{"hello":"world"}"#);
        let mut r = FrameReader::new();
        for byte in &wire {
            r.feed(&[*byte]);
        }
        assert_eq!(r.next_message().as_deref(), Some(r#"{"hello":"world"}"#));
    }

    #[test]
    fn reads_multiple_messages() {
        let mut r = FrameReader::new();
        r.feed(&encode("1"));
        r.feed(&encode("2"));
        assert_eq!(r.next_message().as_deref(), Some("1"));
        assert_eq!(r.next_message().as_deref(), Some("2"));
    }

    #[test]
    fn waits_for_full_body() {
        let mut r = FrameReader::new();
        r.feed(b"Content-Length: 9\r\n\r\n{\"a\":1}"); // 7 of 9 body bytes
        assert_eq!(r.next_message(), None);
        r.feed(b"23"); // now 9 bytes: {"a":1}23
        assert_eq!(r.next_message().as_deref(), Some(r#"{"a":1}23"#));
    }
}
