//! A Server-Sent Events parser tolerant of arbitrary network fragmentation.
//!
//! The decoder is fed raw byte chunks (which may split a UTF-8 character, a
//! line, or an event boundary anywhere) and yields complete [`SseEvent`]s. It
//! follows the SSE framing rules that matter for LLM providers:
//!
//! - Lines are separated by `\n`, `\r\n`, or `\r`.
//! - A field is `name: value` (a single leading space after the colon is
//!   stripped). A line with no colon is a field with an empty value.
//! - Lines beginning with `:` are comments and ignored.
//! - A blank line dispatches the accumulated event.
//! - Multiple `data:` fields in one event are joined with `\n`.
//!
//! The parser never assumes a chunk boundary aligns with anything.

/// A dispatched SSE event: its `event:` type (if any) and joined `data` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Incremental SSE decoder. Hold one per stream and feed it bytes.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes received but not yet forming a complete line.
    buffer: Vec<u8>,
    /// The `event:` value for the event currently being assembled.
    current_event: Option<String>,
    /// The `data:` lines accumulated for the current event.
    data_lines: Vec<String>,
    /// True if any field has been seen since the last dispatch.
    has_fields: bool,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a raw byte chunk; returns any events completed by this chunk.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        // Extract complete lines. We look for `\n`; `\r` is normalized. A lone
        // trailing `\r` at the very end of the buffer is held back in case the
        // next chunk starts with `\n` (a split `\r\n`).
        loop {
            let Some(nl) = self.buffer.iter().position(|&b| b == b'\n') else {
                // No newline; but a completed line could still be `\r`-terminated.
                if let Some(event) = self.try_split_cr() {
                    if let Some(ev) = event {
                        events.push(ev);
                    }
                    continue;
                }
                break;
            };

            let mut line: Vec<u8> = self.buffer.drain(..=nl).collect();
            line.pop(); // remove '\n'
            if line.last() == Some(&b'\r') {
                line.pop(); // remove '\r' of a '\r\n' pair
            }
            if let Some(ev) = self.process_line(&line) {
                events.push(ev);
            }
        }

        events
    }

    /// Handle `\r`-only line terminators that appear without a following `\n`.
    /// Returns `Some(None)` if a line was processed but produced no event,
    /// `Some(Some(ev))` if it dispatched, and `None` if there is nothing to do.
    fn try_split_cr(&mut self) -> Option<Option<SseEvent>> {
        // Find a `\r` that is not the final byte (a final `\r` might be the first
        // half of a `\r\n` still in flight, so we wait for more input).
        let pos = self.buffer.iter().position(|&b| b == b'\r')?;
        if pos == self.buffer.len() - 1 {
            return None;
        }
        let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
        line.pop(); // remove '\r'
        Some(self.process_line(&line))
    }

    /// Process one complete logical line (without its terminator).
    fn process_line(&mut self, line: &[u8]) -> Option<SseEvent> {
        // Blank line: dispatch the current event (if any fields were seen).
        if line.is_empty() {
            return self.dispatch();
        }

        // Comment line.
        if line.first() == Some(&b':') {
            return None;
        }

        let text = String::from_utf8_lossy(line);
        let (field, value) = match text.split_once(':') {
            Some((f, v)) => {
                // A single leading space in the value is stripped.
                let v = v.strip_prefix(' ').unwrap_or(v);
                (f, v)
            }
            None => (text.as_ref(), ""),
        };

        self.has_fields = true;
        match field {
            "data" => self.data_lines.push(value.to_string()),
            "event" => self.current_event = Some(value.to_string()),
            // id / retry / unknown fields are irrelevant to LLM streaming.
            _ => {}
        }
        None
    }

    /// Emit the accumulated event and reset. Returns `None` for empty dispatches
    /// (e.g. leading blank lines) so callers are not spammed.
    fn dispatch(&mut self) -> Option<SseEvent> {
        if !self.has_fields {
            return None;
        }
        let event = self.current_event.take();
        let data = self.data_lines.join("\n");
        self.data_lines.clear();
        self.has_fields = false;
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(decoder: &mut SseDecoder, chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut out = Vec::new();
        for c in chunks {
            out.extend(decoder.feed(c));
        }
        out
    }

    #[test]
    fn parses_single_event() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn joins_multiple_data_lines() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: a\ndata: b\n\n");
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn tolerates_byte_level_fragmentation() {
        let mut d = SseDecoder::new();
        // Feed one byte at a time.
        let input = b"data: {\"x\":1}\n\n";
        let mut events = Vec::new();
        for &byte in input {
            events.extend(d.feed(&[byte]));
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"x\":1}");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: hi\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hi");
    }

    #[test]
    fn handles_split_crlf_across_chunks() {
        let mut d = SseDecoder::new();
        let events = feed_all(&mut d, &[b"data: hi\r", b"\n\r", b"\n"]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hi");
    }

    #[test]
    fn ignores_comments_and_blank_leading_lines() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"\n: this is a comment\ndata: real\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn preserves_json_with_colons_in_value() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: {\"a\":\"b:c\"}\n\n");
        assert_eq!(events[0].data, "{\"a\":\"b:c\"}");
    }

    #[test]
    fn captures_event_field() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"event: done\ndata: {}\n\n");
        assert_eq!(events[0].event.as_deref(), Some("done"));
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data: 1\n\ndata: 2\n\ndata: 3\n\n");
        let datas: Vec<_> = events.iter().map(|e| e.data.as_str()).collect();
        assert_eq!(datas, ["1", "2", "3"]);
    }

    #[test]
    fn value_without_colon_is_empty() {
        let mut d = SseDecoder::new();
        let events = d.feed(b"data\n\n");
        assert_eq!(events[0].data, "");
    }
}
