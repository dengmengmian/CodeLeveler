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

use std::fmt;

/// Maximum bytes in one unterminated SSE line.
pub const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;
/// Maximum joined `data:` payload bytes in one SSE event.
pub const MAX_SSE_EVENT_DATA_BYTES: usize = 8 * 1024 * 1024;

/// A dispatched SSE event: its `event:` type (if any) and joined `data` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// A size-limit violation while decoding an SSE stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseDecodeError {
    LineTooLong { limit: usize },
    EventDataTooLarge { limit: usize },
}

impl fmt::Display for SseDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineTooLong { limit } => {
                write!(f, "SSE line exceeded the {limit}-byte limit")
            }
            Self::EventDataTooLarge { limit } => {
                write!(f, "SSE event data exceeded the {limit}-byte limit")
            }
        }
    }
}

impl std::error::Error for SseDecodeError {}

/// Incremental SSE decoder. Hold one per stream and feed it bytes.
#[derive(Debug)]
pub struct SseDecoder {
    /// Bytes received but not yet forming a complete line. Terminators are not stored.
    buffer: Vec<u8>,
    /// A trailing `\r` is held until the next byte to distinguish `\r` from `\r\n`.
    pending_cr: bool,
    /// The `event:` value for the event currently being assembled.
    current_event: Option<String>,
    /// Joined `data:` payload for the current event.
    data: String,
    has_data: bool,
    max_event_data_bytes: usize,
    /// True if any field has been seen since the last dispatch.
    has_fields: bool,
    /// A failed decoder stays failed so callers cannot accidentally continue it.
    failed: Option<SseDecodeError>,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self {
            buffer: Vec::new(),
            pending_cr: false,
            current_event: None,
            data: String::new(),
            has_data: false,
            max_event_data_bytes: MAX_SSE_EVENT_DATA_BYTES,
            has_fields: false,
            failed: None,
        }
    }
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_event_data_limit(limit: usize) -> Self {
        Self {
            max_event_data_bytes: limit,
            ..Self::default()
        }
    }

    /// Feed a raw byte chunk, preserving the original infallible API.
    ///
    /// Size-limit failures poison and clear the decoder and produce no events.
    /// New stream consumers should use [`Self::try_feed`] to surface the reason.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.try_feed(chunk).unwrap_or_default()
    }

    /// Feed a raw byte chunk; returns any events completed by this chunk.
    ///
    /// Lines and event data are bounded so a peer cannot grow decoder memory
    /// indefinitely by withholding a terminator or event boundary.
    pub fn try_feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseDecodeError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }

        let mut events = Vec::new();
        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if let Some(event) = self.process_buffered_line()? {
                    events.push(event);
                }
                if byte == b'\n' {
                    continue;
                }
            }

            match byte {
                b'\n' => {
                    if let Some(event) = self.process_buffered_line()? {
                        events.push(event);
                    }
                }
                b'\r' => self.pending_cr = true,
                _ => {
                    if self.buffer.len() == MAX_SSE_LINE_BYTES {
                        return self.fail(SseDecodeError::LineTooLong {
                            limit: MAX_SSE_LINE_BYTES,
                        });
                    }
                    self.buffer.push(byte);
                }
            }
        }

        Ok(events)
    }

    fn fail<T>(&mut self, error: SseDecodeError) -> Result<T, SseDecodeError> {
        self.buffer.clear();
        self.current_event = None;
        self.data.clear();
        self.has_data = false;
        self.has_fields = false;
        self.failed = Some(error.clone());
        Err(error)
    }

    fn process_buffered_line(&mut self) -> Result<Option<SseEvent>, SseDecodeError> {
        let line = std::mem::take(&mut self.buffer);
        self.process_line(&line)
    }

    /// Process one complete logical line (without its terminator).
    fn process_line(&mut self, line: &[u8]) -> Result<Option<SseEvent>, SseDecodeError> {
        // Blank line: dispatch the current event (if any fields were seen).
        if line.is_empty() {
            return Ok(self.dispatch());
        }

        // Comment line.
        if line.first() == Some(&b':') {
            return Ok(None);
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
            "data" => {
                let separator = usize::from(self.has_data);
                let Some(next_size) = self
                    .data
                    .len()
                    .checked_add(separator)
                    .and_then(|n| n.checked_add(value.len()))
                else {
                    return self.fail(SseDecodeError::EventDataTooLarge {
                        limit: self.max_event_data_bytes,
                    });
                };
                if next_size > self.max_event_data_bytes {
                    return self.fail(SseDecodeError::EventDataTooLarge {
                        limit: self.max_event_data_bytes,
                    });
                }
                if separator != 0 {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.current_event = Some(value.to_string()),
            // id / retry / unknown fields are irrelevant to LLM streaming.
            _ => {}
        }
        Ok(None)
    }

    /// Emit the accumulated event and reset. Returns `None` for empty dispatches
    /// (e.g. leading blank lines) so callers are not spammed.
    fn dispatch(&mut self) -> Option<SseEvent> {
        if !self.has_fields {
            return None;
        }
        let event = self.current_event.take();
        let data = std::mem::take(&mut self.data);
        self.has_data = false;
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
            out.extend(decoder.try_feed(c).unwrap());
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
        let events = d.try_feed(b"data: a\ndata: b\n\n").unwrap();
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn tolerates_byte_level_fragmentation() {
        let mut d = SseDecoder::new();
        let input = b"data: {\"x\":1}\n\n";
        let mut events = Vec::new();
        for &byte in input {
            events.extend(d.try_feed(&[byte]).unwrap());
        }
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"x\":1}");
    }

    #[test]
    fn handles_crlf_line_endings() {
        let mut d = SseDecoder::new();
        let events = d.try_feed(b"data: hi\r\n\r\n").unwrap();
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
    fn handles_cr_only_line_endings() {
        let mut d = SseDecoder::new();
        let events = d.try_feed(b"data: hi\r\rnext").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hi");
    }

    #[test]
    fn ignores_comments_and_blank_leading_lines() {
        let mut d = SseDecoder::new();
        let events = d
            .try_feed(b"\n: this is a comment\ndata: real\n\n")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn preserves_json_with_colons_in_value() {
        let mut d = SseDecoder::new();
        let events = d.try_feed(b"data: {\"a\":\"b:c\"}\n\n").unwrap();
        assert_eq!(events[0].data, "{\"a\":\"b:c\"}");
    }

    #[test]
    fn captures_event_field() {
        let mut d = SseDecoder::new();
        let events = d.try_feed(b"event: done\ndata: {}\n\n").unwrap();
        assert_eq!(events[0].event.as_deref(), Some("done"));
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let mut d = SseDecoder::new();
        let events = d.try_feed(b"data: 1\n\ndata: 2\n\ndata: 3\n\n").unwrap();
        let datas: Vec<_> = events.iter().map(|e| e.data.as_str()).collect();
        assert_eq!(datas, ["1", "2", "3"]);
    }

    #[test]
    fn value_without_colon_is_empty() {
        let mut d = SseDecoder::new();
        let events = d.try_feed(b"data\n\n").unwrap();
        assert_eq!(events[0].data, "");
    }

    #[test]
    fn rejects_an_unterminated_line_over_the_limit() {
        let mut d = SseDecoder::new();
        d.try_feed(&vec![b'x'; MAX_SSE_LINE_BYTES]).unwrap();
        let err = d.try_feed(b"x").unwrap_err();
        assert_eq!(
            err,
            SseDecodeError::LineTooLong {
                limit: MAX_SSE_LINE_BYTES
            }
        );
        assert_eq!(d.try_feed(b"data: ignored\n\n").unwrap_err(), err);
    }

    #[test]
    fn accepts_a_line_exactly_at_the_limit() {
        let mut d = SseDecoder::new();
        let mut line = vec![b'x'; MAX_SSE_LINE_BYTES];
        line.push(b'\n');
        d.try_feed(&line).unwrap();
    }

    #[test]
    fn rejects_cumulative_event_data_over_the_limit() {
        let mut d = SseDecoder::with_event_data_limit(7);
        d.try_feed(b"data: abc\ndata: def\n").unwrap();
        let err = d.try_feed(b"data: x\n").unwrap_err();
        assert_eq!(err, SseDecodeError::EventDataTooLarge { limit: 7 });
    }

    #[test]
    fn empty_data_lines_are_bounded_by_the_same_payload_buffer() {
        let mut d = SseDecoder::with_event_data_limit(8);
        // The first empty value uses zero bytes; each join newline after it uses one.
        d.try_feed(b"data:\ndata:\ndata:\ndata:\ndata:\ndata:\ndata:\ndata:\ndata:\n")
            .unwrap();
        let err = d.try_feed(b"data:\n").unwrap_err();
        assert_eq!(err, SseDecodeError::EventDataTooLarge { limit: 8 });
    }
}
