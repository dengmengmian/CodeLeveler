//! Bounded, read-only file access. Every open in this module is read-only.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::model::ForeignSessionError;
use crate::walk::MAX_LINE_BYTES;

/// One line of a foreign store, with the amount actually retained.
pub(crate) struct SourceLine {
    pub number: u64,
    pub bytes: Vec<u8>,
    /// The line exceeded [`MAX_LINE_BYTES`]; `bytes` is only its prefix.
    pub truncated: bool,
}

/// Stream lines without ever buffering more than [`MAX_LINE_BYTES`].
///
/// The visitor returns whether streaming continues; `false` stops the read
/// immediately, without scanning the rest of the file.
pub(crate) fn for_each_line(
    path: &Path,
    mut visit: impl FnMut(SourceLine) -> bool,
) -> Result<(), ForeignSessionError> {
    let file = File::open(path).map_err(|error| crate::io_error(path, error))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut number = 0u64;
    loop {
        let mut bytes = Vec::new();
        let mut truncated = false;
        let mut consumed = 0usize;
        loop {
            let available = reader
                .fill_buf()
                .map_err(|error| crate::io_error(path, error))?;
            if available.is_empty() {
                break;
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(available.len(), |position| position + 1);
            if !truncated {
                let room = MAX_LINE_BYTES.saturating_sub(bytes.len());
                let keep = room.min(take);
                bytes.extend_from_slice(&available[..keep]);
                truncated = keep < take;
            }
            consumed += take;
            reader.consume(take);
            if newline.is_some() {
                break;
            }
        }
        if consumed == 0 {
            break;
        }
        number += 1;
        while bytes
            .last()
            .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
        {
            bytes.pop();
        }
        if !visit(SourceLine {
            number,
            bytes,
            truncated,
        }) {
            break;
        }
    }
    Ok(())
}

/// Read only the head of a file, up to `max_bytes`, with a record cap.
pub(crate) fn head_lines(
    path: &Path,
    max_bytes: u64,
    max_records: usize,
) -> Result<Vec<(u64, serde_json::Value)>, ForeignSessionError> {
    let mut out = Vec::new();
    let mut read = 0u64;
    for_each_line(path, |line| {
        if read >= max_bytes || out.len() >= max_records {
            return false;
        }
        read += line.bytes.len() as u64 + 1;
        if line.truncated {
            return true;
        }
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line.bytes) {
            out.push((line.number, value));
        }
        true
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::head_lines;

    /// A head read must stop once its caps are reached, not scan to EOF. A
    /// named pipe is the probe: the writer keeps the pipe open and sends
    /// nothing after the head, so only a reader that stops on its own returns.
    #[test]
    #[cfg(unix)]
    fn head_lines_stops_at_the_record_cap_instead_of_waiting_for_eof() {
        let dir = tempfile::tempdir().expect("temp dir");
        let fifo = dir.path().join("feed");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed");

        let (release_tx, release_rx) = mpsc::channel::<()>();
        let writer = std::thread::spawn({
            let fifo = fifo.clone();
            move || {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&fifo)
                    .expect("open fifo for writing");
                for index in 0..8 {
                    writeln!(file, "{{\"n\":{index}}}").expect("write record");
                }
                file.flush().expect("flush");
                let _ = release_rx.recv_timeout(Duration::from_secs(30));
            }
        });

        let (result_tx, result_rx) = mpsc::channel();
        let reader = std::thread::spawn({
            let fifo = fifo.clone();
            move || {
                let started = Instant::now();
                let count = head_lines(&fifo, 1024 * 1024, 4).map(|lines| lines.len());
                let _ = result_tx.send((count, started.elapsed()));
            }
        });

        let outcome = result_rx.recv_timeout(Duration::from_secs(5));
        let _ = release_tx.send(());
        let _ = writer.join();
        let _ = reader.join();

        let (count, elapsed) =
            outcome.expect("head_lines waited for EOF instead of stopping at the cap");
        assert_eq!(count.expect("head lines"), 4);
        assert!(
            elapsed < Duration::from_secs(5),
            "head_lines took {elapsed:?} for a bounded head"
        );
    }
}
