//! Deterministic stream record/replay for the TUI.
//!
//! Recording is opt-in: `LEVELER_TUI_RECORD=/path/to/stream.jsonl` makes the
//! event loop append every received [`RuntimeEvent`] as one JSON line with a
//! monotonic offset. Nothing is recorded unless the variable is set, so a
//! normal session writes no file and pays one `Option` check per event.
//!
//! A recording is the input to a replay harness (tests / benchmarks): applying
//! the same event sequence through the same reducer and renderer removes the
//! provider, network, and model from the measurement entirely.
//!
//! Privacy: a `RuntimeEvent` carries conversation text and tool arguments, so a
//! recording is as sensitive as the session. It contains no API keys and no
//! environment, but it must be reviewed before it is committed anywhere.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use leveler_client_protocol::RuntimeEvent;

const ENV_RECORD: &str = "LEVELER_TUI_RECORD";

/// One recorded event, in replay order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedEvent {
    /// Monotonic 0-based sequence within the recording.
    pub seq: u64,
    /// Offset from the first recorded event, in microseconds.
    pub offset_us: u64,
    pub event: RuntimeEvent,
}

/// Appends received events to a JSONL file. Drop flushes.
pub struct Recorder {
    writer: BufWriter<File>,
    start: Instant,
    seq: u64,
}

impl Recorder {
    /// Open a recorder when `LEVELER_TUI_RECORD` names a path. A path that
    /// cannot be opened yields `None` (profiling/recording must never break the
    /// UI); the failure is surfaced on stderr.
    pub fn from_env() -> Option<Self> {
        let path = leveler_core::environment().var(ENV_RECORD)?;
        if path.is_empty() {
            return None;
        }
        match File::create(&path) {
            Ok(file) => Some(Self {
                writer: BufWriter::new(file),
                start: Instant::now(),
                seq: 0,
            }),
            Err(error) => {
                eprintln!("LEVELER_TUI_RECORD: cannot open {path}: {error}");
                None
            }
        }
    }

    /// Append one event. Serialization failures are ignored (recording is
    /// best-effort; it must never disturb the session).
    pub fn record(&mut self, event: &RuntimeEvent) {
        let row = RecordedEvent {
            seq: self.seq,
            offset_us: self.start.elapsed().as_micros() as u64,
            event: event.clone(),
        };
        self.seq += 1;
        if serde_json::to_writer(&mut self.writer, &row).is_ok() {
            let _ = self.writer.write_all(b"\n");
        }
    }
}

/// Read a recording back, in file order. A malformed line is a hard error: a
/// replayed stream must never silently skip an event.
pub fn read_stream(path: &Path) -> std::io::Result<Vec<RecordedEvent>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let row: RecordedEvent = serde_json::from_str(&line).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unreplayable stream line: {error}"),
            )
        })?;
        out.push(row);
    }
    Ok(out)
}

/// Write a recording, for fixture generation and round-trip tests.
pub fn write_stream(path: &Path, events: &[RecordedEvent]) -> std::io::Result<()> {
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    for row in events {
        serde_json::to_writer(&mut writer, row)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::{MessageId, SessionId};

    #[test]
    fn recording_round_trips_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.jsonl");
        let events = vec![
            RecordedEvent {
                seq: 0,
                offset_us: 0,
                event: RuntimeEvent::AssistantMessageStarted {
                    message_id: MessageId::new("m1"),
                },
            },
            RecordedEvent {
                seq: 1,
                offset_us: 1500,
                event: RuntimeEvent::AssistantTextDelta {
                    message_id: MessageId::new("m1"),
                    delta: "hello".into(),
                },
            },
        ];
        write_stream(&path, &events).unwrap();
        let read = read_stream(&path).unwrap();
        assert_eq!(read, events);
    }

    #[test]
    fn no_env_means_no_recorder() {
        // The environment snapshot in a unit test is empty, so recording is off.
        assert!(Recorder::from_env().is_none());
        let _ = SessionId::new("s1");
    }
}
