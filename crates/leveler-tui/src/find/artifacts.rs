//! Bounded, cancellable search of a saved tool result on disk.
//!
//! A tool artifact is the one piece of session evidence that can legitimately
//! be hundreds of megabytes. This scanner never materialises it: it reads a
//! fixed-size window, carries just enough tail across the boundary for a match
//! to be found exactly once, and stops as soon as the caller's search
//! generation moves on. Line numbers are counted from the bytes read, not from
//! a second index built for the search.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::find::matching;

/// Bytes read per step. Large enough that a 50 MiB capture is ~200 reads,
/// small enough that cancellation is observed promptly.
pub const ARTIFACT_CHUNK_BYTES: usize = 256 * 1024;
/// Matches one artifact may contribute before the scan stops early.
pub const MAX_ARTIFACT_HITS: usize = 200;
/// Characters of context kept on each side of a match in a result row.
const SNIPPET_CONTEXT: usize = 80;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactHit {
    /// One-based logical line number.
    pub line: usize,
    /// Byte offset of the match in the artifact (diagnostic; the line is the
    /// navigation identity).
    pub byte_offset: u64,
    pub snippet: String,
    pub match_start: usize,
    pub match_end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactStatus {
    /// Every byte was read.
    Scanned,
    /// The per-artifact hit cap was reached; later bytes were not searched.
    Capped,
    /// The file could not be read. Never presented as "no matches".
    ReadError(String),
    /// A newer search replaced this one.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    pub hits: Vec<ArtifactHit>,
    pub status: ArtifactStatus,
    pub bytes_scanned: u64,
}

/// Search one artifact without loading it whole.
pub fn scan_file(path: &Path, query: &str, cancel: &AtomicU64, generation: u64) -> ScanOutcome {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => {
            return ScanOutcome {
                hits: Vec::new(),
                status: ArtifactStatus::ReadError(error.to_string()),
                bytes_scanned: 0,
            };
        }
    };
    scan_reader(
        file,
        query,
        cancel,
        generation,
        ARTIFACT_CHUNK_BYTES,
        MAX_ARTIFACT_HITS,
    )
}

fn scan_reader(
    mut reader: impl Read,
    query: &str,
    cancel: &AtomicU64,
    generation: u64,
    chunk_bytes: usize,
    hit_limit: usize,
) -> ScanOutcome {
    let mut hits = Vec::new();
    let mut status = ArtifactStatus::Scanned;
    let mut bytes_scanned = 0u64;
    let chunk_bytes = chunk_bytes.max(1);
    let mut buffer = vec![0u8; chunk_bytes];
    // Raw bytes of an incomplete UTF-8 sequence at the previous chunk's end.
    let mut raw_carry: Vec<u8> = Vec::new();
    // Decoded tail kept only for matches that cross a chunk boundary.
    let mut text_carry = String::new();
    let mut line_at_text_start = 1usize;
    let mut file_offset = 0u64;

    loop {
        if cancel.load(Ordering::Relaxed) != generation {
            return ScanOutcome {
                hits,
                status: ArtifactStatus::Cancelled,
                bytes_scanned,
            };
        }
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) => {
                return ScanOutcome {
                    hits,
                    status: ArtifactStatus::ReadError(error.to_string()),
                    bytes_scanned,
                };
            }
        };
        if read == 0 {
            break;
        }
        bytes_scanned += read as u64;
        let decoded = decode(&mut raw_carry, &buffer[..read]);
        let carry_len = text_carry.len();
        let text = format!("{text_carry}{decoded}");

        let matches = matching::find_all(&text, query, hit_limit.saturating_sub(hits.len()).max(1));
        let mut newlines_before = 0usize;
        let mut counted_to = 0usize;
        for found in matches {
            // A match wholly inside the carry was already reported by the
            // previous chunk. One that ends past it was not: the previous
            // chunk lacked its tail.
            if found.end <= carry_len {
                continue;
            }
            newlines_before += count_newlines(&text[counted_to..found.start]);
            counted_to = found.start;
            let line = line_at_text_start + newlines_before;
            let (snippet, match_start, match_end) =
                matching::snippet(&text, found, SNIPPET_CONTEXT);
            hits.push(ArtifactHit {
                line,
                byte_offset: file_offset
                    .saturating_add(found.start as u64)
                    .saturating_sub(carry_len as u64),
                snippet,
                match_start,
                match_end,
            });
            if hits.len() >= hit_limit {
                status = ArtifactStatus::Capped;
                return ScanOutcome {
                    hits,
                    status,
                    bytes_scanned,
                };
            }
        }

        // Keep only the tail a future match can still depend on. The folded
        // needle is at most four bytes per query byte; this is comfortably
        // larger and still bounded.
        let keep_bytes = query.len().saturating_mul(4).saturating_add(32).max(64);
        let cut = char_boundary_at_or_before(&text, text.len().saturating_sub(keep_bytes));
        line_at_text_start += count_newlines(&text[..cut]);
        text_carry = text[cut..].to_string();
        file_offset += read as u64;
    }

    ScanOutcome {
        hits,
        status,
        bytes_scanned,
    }
}

/// Decode as much valid UTF-8 as possible, replacing invalid sequences with
/// the replacement character and carrying only an incomplete trailing
/// sequence into the next chunk.
fn decode(raw_carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    let mut data = std::mem::take(raw_carry);
    data.extend_from_slice(chunk);
    let mut out = String::with_capacity(data.len());
    let mut at = 0usize;
    while at < data.len() {
        match std::str::from_utf8(&data[at..]) {
            Ok(text) => {
                out.push_str(text);
                at = data.len();
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // SAFETY of the slice: `from_utf8` just proved this prefix.
                    out.push_str(std::str::from_utf8(&data[at..at + valid]).unwrap_or(""));
                    at += valid;
                }
                match error.error_len() {
                    Some(len) => {
                        out.push('\u{FFFD}');
                        at += len;
                    }
                    None => {
                        // Incomplete sequence at the end: keep it for the next
                        // chunk instead of turning it into a replacement char.
                        break;
                    }
                }
            }
        }
    }
    *raw_carry = data[at..].to_vec();
    out
}

fn count_newlines(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count()
}

fn char_boundary_at_or_before(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Arc;

    struct Chunked {
        data: Vec<u8>,
        at: usize,
        chunk: usize,
    }

    impl Read for Chunked {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let remaining = self.data.len().saturating_sub(self.at);
            let take = remaining.min(self.chunk).min(buf.len());
            buf[..take].copy_from_slice(&self.data[self.at..self.at + take]);
            self.at += take;
            Ok(take)
        }
    }

    fn scan_bytes(data: &[u8], query: &str, chunk: usize, limit: usize) -> ScanOutcome {
        let cancel = AtomicU64::new(1);
        scan_reader(
            Chunked {
                data: data.to_vec(),
                at: 0,
                chunk,
            },
            query,
            &cancel,
            1,
            chunk,
            limit,
        )
    }

    #[test]
    fn finds_matches_with_one_based_lines() {
        let data = b"alpha\nneedle here\nomega\nNEEDLE again\n";
        let outcome = scan_bytes(data, "needle", 4096, 10);
        assert_eq!(outcome.status, ArtifactStatus::Scanned);
        assert_eq!(outcome.hits.len(), 2);
        assert_eq!(outcome.hits[0].line, 2);
        assert_eq!(outcome.hits[1].line, 4);
        assert!(outcome.hits[0].snippet.contains("needle here"));
        assert!(outcome.hits[1].snippet.contains("NEEDLE again"));
        for hit in &outcome.hits {
            assert_eq!(
                &hit.snippet[hit.match_start..hit.match_end].to_lowercase(),
                "needle"
            );
        }
    }

    #[test]
    fn a_match_across_a_chunk_boundary_is_found_once() {
        // The boundary falls inside "refund": chunk 1 ends after "xxxrefu", so
        // the match only exists once the carried tail is prepended.
        let data = b"line-one\nxxxrefund-after\n";
        let outcome = scan_bytes(data, "refund", 16, 10);
        assert_eq!(outcome.status, ArtifactStatus::Scanned);
        assert_eq!(outcome.hits.len(), 1, "{:?}", outcome.hits);
        assert_eq!(outcome.hits[0].line, 2);
    }

    #[test]
    fn line_numbers_survive_many_chunks() {
        let mut data = String::new();
        for index in 1..=5000 {
            data.push_str(&format!("row {index}\n"));
        }
        data.push_str("the needle line\n");
        let outcome = scan_bytes(data.as_bytes(), "needle", 97, 10);
        assert_eq!(outcome.status, ArtifactStatus::Scanned);
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].line, 5001);
    }

    #[test]
    fn a_missing_file_is_a_read_error_not_an_empty_result() {
        let cancel = AtomicU64::new(1);
        let outcome = scan_file(
            Path::new("/definitely/not/a/leveler/artifact.txt"),
            "anything",
            &cancel,
            1,
        );
        assert!(matches!(outcome.status, ArtifactStatus::ReadError(_)));
        assert!(outcome.hits.is_empty());
    }

    #[test]
    fn a_stale_generation_stops_before_reading() {
        let cancel = AtomicU64::new(9);
        let outcome = scan_reader(
            Cursor::new(b"needle".to_vec()),
            "needle",
            &cancel,
            1,
            64,
            10,
        );
        assert_eq!(outcome.status, ArtifactStatus::Cancelled);
        assert_eq!(outcome.bytes_scanned, 0);
        assert!(outcome.hits.is_empty());
    }

    #[test]
    fn the_hit_cap_stops_the_scan_and_says_so() {
        let data = b"needle\nneedle\nneedle\nneedle\n";
        let outcome = scan_bytes(data, "needle", 4096, 3);
        assert_eq!(outcome.status, ArtifactStatus::Capped);
        assert_eq!(outcome.hits.len(), 3);
        assert!(outcome.bytes_scanned < data.len() as u64 * 2);
    }

    #[test]
    fn invalid_utf8_does_not_panic_or_hide_a_later_match() {
        let mut data = b"before ".to_vec();
        data.extend_from_slice(&[0xff, 0xfe, 0x80]);
        data.extend_from_slice(b" after\nneedle\n");
        let outcome = scan_bytes(&data, "needle", 7, 10);
        assert_eq!(outcome.status, ArtifactStatus::Scanned);
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].line, 2);
    }

    #[test]
    fn a_multi_megabyte_artifact_is_scanned_in_bounded_time() {
        let root = std::env::temp_dir().join(format!("leveler-find-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("big.txt");
        let mut data = String::new();
        for line in 1..=100_000 {
            if line == 90_000 {
                data.push_str("the needle line with a refund token\n");
            } else {
                data.push_str(
                    "0123456789012345678901234567890123456789012345678901234567890123456789\n",
                );
            }
        }
        std::fs::write(&path, &data).unwrap();
        let cancel = AtomicU64::new(1);
        let started = std::time::Instant::now();
        let outcome = scan_file(&path, "refund token", &cancel, 1);
        let elapsed = started.elapsed();
        println!(
            "artifact perf: bytes={} scan_ms={} hits={} status={:?}",
            outcome.bytes_scanned,
            elapsed.as_millis(),
            outcome.hits.len(),
            outcome.status
        );
        assert_eq!(outcome.bytes_scanned, data.len() as u64);
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].line, 90_000);
        assert!(elapsed.as_secs_f64() < 10.0, "scan took {elapsed:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Cancellation is checked BETWEEN chunks, so a newer query stops the
    /// scan even when the reader itself has more data ready.
    #[test]
    fn a_generation_change_stops_the_scan_between_chunks() {
        struct Changing {
            data: Vec<u8>,
            at: usize,
            cancel: Arc<AtomicU64>,
            reads: usize,
        }
        impl Read for Changing {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let take = self
                    .data
                    .len()
                    .saturating_sub(self.at)
                    .min(64)
                    .min(buf.len());
                buf[..take].copy_from_slice(&self.data[self.at..self.at + take]);
                self.at += take;
                self.reads += 1;
                if self.reads == 2 {
                    self.cancel.store(2, Ordering::Relaxed);
                }
                Ok(take)
            }
        }
        let cancel = Arc::new(AtomicU64::new(1));
        let data = vec![b'a'; 10_000];
        let outcome = scan_reader(
            Changing {
                data: data.clone(),
                at: 0,
                cancel: cancel.clone(),
                reads: 0,
            },
            "zzz",
            &cancel,
            1,
            64,
            10,
        );
        assert_eq!(outcome.status, ArtifactStatus::Cancelled);
        assert!(outcome.bytes_scanned < data.len() as u64);
        assert!(outcome.bytes_scanned > 0);
    }

    #[test]
    fn snippets_are_single_line() {
        let data = b"alpha\nneedle with\nmore text\n";
        let outcome = scan_bytes(data, "needle", 4096, 10);
        let hit = &outcome.hits[0];
        assert!(!hit.snippet.contains('\n'), "{:?}", hit.snippet);
    }
}
