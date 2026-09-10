//! `WorkspaceReader` — bounded, deterministic file reading.
//!
//! Owns exactly one job: turn a path and a line window into bounded text plus
//! the observation a later guarded mutation checks against. It holds no
//! behaviour policy, no output-budget policy and no edit policy; the tool
//! adapter renders the result and the harness decides everything else
//! (`docs/ARCHITECTURE.md` §1.1, §18.3 A).

use tokio::io::AsyncBufReadExt;
use tokio_util::sync::CancellationToken;

use leveler_context::ContentFingerprint;

/// How much of a file to read, and how much output the caller can hold.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadWindow {
    /// 1-based first line to include.
    pub(crate) start_line: usize,
    /// 1-based last line to include, inclusive.
    pub(crate) end_line: usize,
    /// Ceiling on the bytes the caller will render for this window.
    pub(crate) max_bytes: usize,
    /// Bytes the caller's renderer adds to a line beyond the line's own text,
    /// as a function of the line number. Without it the reader budgets the
    /// file's bytes while the caller emits something longer, and the result
    /// overshoots by exactly the decoration.
    pub(crate) per_line_overhead: fn(usize) -> usize,
}

impl ReadWindow {
    /// A window for a caller that renders each line verbatim.
    #[cfg(test)]
    pub(crate) fn new(
        start_line: Option<usize>,
        end_line: Option<usize>,
        max_bytes: usize,
    ) -> Self {
        Self::rendered(start_line, end_line, max_bytes, |_| 1)
    }

    /// A window for a caller that decorates each line — line numbers, say.
    pub(crate) fn rendered(
        start_line: Option<usize>,
        end_line: Option<usize>,
        max_bytes: usize,
        per_line_overhead: fn(usize) -> usize,
    ) -> Self {
        Self {
            start_line: start_line.unwrap_or(1).max(1),
            end_line: end_line.unwrap_or(usize::MAX),
            max_bytes: max_bytes.max(1),
            per_line_overhead,
        }
    }
}

/// The version of a file as the reader observed it. This is what stale-write
/// protection compares against later; it is deliberately separate from the
/// content the model was shown, because the two have different sizes and
/// different lifetimes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReadObservation {
    /// Fingerprint over the complete file, not just the window.
    pub(crate) fingerprint: u64,
    /// Total lines in the file.
    pub(crate) total_lines: usize,
    /// Size of the file on disk.
    pub(crate) file_bytes: u64,
}

/// Why the returned content stops short of the requested window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Clip {
    /// The whole requested window fits.
    None,
    /// Stopped at a line boundary; `next_line` is the first line not shown.
    AtLine { next_line: usize },
    /// One line was longer than the remaining budget and was cut mid-line.
    InsideLine { line: usize, line_bytes: usize },
}

/// A bounded read: the rendered window, how it was clipped, and the version
/// observed while producing it.
#[derive(Debug)]
pub(crate) struct BoundedRead {
    /// `(line number, text)` for each line in the window, without line endings.
    pub(crate) lines: Vec<(usize, String)>,
    pub(crate) clip: Clip,
    pub(crate) observation: ReadObservation,
}

/// Mechanical reasons a read cannot produce text.
#[derive(Debug)]
pub(crate) enum ReadError {
    NotFound,
    IsDirectory,
    /// The file is not valid UTF-8; `line` is where decoding first failed.
    NotUtf8 {
        line: usize,
    },
    /// The file contains NUL bytes, so it is not text.
    Binary,
    Cancelled,
    Io(String),
}

/// Reads workspace files. Stateless: the caller owns the resolved path and
/// whatever it does with the observation.
pub(crate) struct WorkspaceReader;

impl WorkspaceReader {
    /// Read `window` from `path`.
    ///
    /// The whole file is streamed once, because the fingerprint that guards a
    /// later edit covers the whole file and there is no cheaper way to produce
    /// it that is equally strong. Memory stays O(longest line + window), so a
    /// bounded window of a very large file is a bounded operation — file size
    /// alone never makes a read impossible.
    pub(crate) async fn read(
        path: &std::path::Path,
        window: ReadWindow,
        cancellation: &CancellationToken,
    ) -> Result<BoundedRead, ReadError> {
        let meta = match tokio::fs::metadata(path).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::NotFound),
            Err(e) => return Err(ReadError::Io(e.to_string())),
        };
        if meta.is_dir() {
            return Err(ReadError::IsDirectory);
        }

        let file = match tokio::fs::File::open(path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ReadError::NotFound),
            Err(e) => return Err(ReadError::Io(e.to_string())),
        };
        let mut reader = tokio::io::BufReader::new(file);
        let mut raw = Vec::new();
        let mut fingerprint = ContentFingerprint::default();

        let mut lines: Vec<(usize, String)> = Vec::new();
        let mut used = 0usize;
        let mut clip = Clip::None;
        let mut total_lines = 0usize;

        loop {
            raw.clear();
            let read = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ReadError::Cancelled),
                read = reader.read_until(b'\n', &mut raw) => read,
            }
            .map_err(|e| ReadError::Io(e.to_string()))?;
            if read == 0 {
                break;
            }
            fingerprint.update(&raw);
            total_lines += 1;

            if raw.contains(&0) {
                return Err(ReadError::Binary);
            }
            // UTF-8 is checked on every line of the whole file, not sampled:
            // the model must never be handed replacement characters as if they
            // were the file's text.
            let text = std::str::from_utf8(strip_eol(&raw))
                .map_err(|_| ReadError::NotUtf8 { line: total_lines })?;

            if matches!(clip, Clip::None) && total_lines >= window.start_line {
                if total_lines > window.end_line {
                    continue;
                }
                let overhead = (window.per_line_overhead)(total_lines);
                match fit(text, used, window.max_bytes, overhead) {
                    Fit::Whole => {
                        used += text.len() + overhead;
                        lines.push((total_lines, text.to_string()));
                    }
                    Fit::Partial(prefix) => {
                        used += prefix.len() + overhead;
                        lines.push((total_lines, prefix.to_string()));
                        clip = Clip::InsideLine {
                            line: total_lines,
                            line_bytes: text.len(),
                        };
                    }
                    Fit::None => {
                        clip = Clip::AtLine {
                            next_line: total_lines,
                        };
                    }
                }
            }
        }

        Ok(BoundedRead {
            lines,
            clip,
            observation: ReadObservation {
                fingerprint: fingerprint.finish(),
                total_lines,
                file_bytes: meta.len(),
            },
        })
    }
}

fn strip_eol(raw: &[u8]) -> &[u8] {
    let mut end = raw;
    if end.ends_with(b"\n") {
        end = &end[..end.len() - 1];
    }
    if end.ends_with(b"\r") {
        end = &end[..end.len() - 1];
    }
    end
}

enum Fit<'a> {
    Whole,
    Partial(&'a str),
    None,
}

/// How much of `text` fits in what is left of the budget, once the caller's
/// per-line decoration is charged against it. A line is only cut mid-way when
/// nothing has been emitted yet, so a single very long line is still partially
/// readable instead of returning nothing.
fn fit<'a>(text: &'a str, used: usize, max_bytes: usize, overhead: usize) -> Fit<'a> {
    let remaining = max_bytes.saturating_sub(used);
    if text.len() + overhead <= remaining {
        return Fit::Whole;
    }
    if used > 0 || remaining <= overhead {
        return Fit::None;
    }
    let take = leveler_core::floor_char_boundary(text, remaining - overhead);
    if take == 0 {
        Fit::None
    } else {
        Fit::Partial(&text[..take])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("leveler-reader-")
            .tempdir()
            .unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn reads_a_window_and_counts_the_whole_file() {
        let (_dir, path) = temp("a.txt", b"one\ntwo\nthree\nfour\n");
        let read = WorkspaceReader::read(
            &path,
            ReadWindow::new(Some(2), Some(3), 4096),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            read.lines,
            vec![(2, "two".to_string()), (3, "three".to_string())]
        );
        assert_eq!(read.observation.total_lines, 4);
        assert_eq!(read.clip, Clip::None);
    }

    #[tokio::test]
    async fn a_window_past_the_budget_start_is_still_reachable() {
        let mut body = String::new();
        for i in 0..5_000 {
            body.push_str(&format!("line {i} {}\n", "y".repeat(60)));
        }
        let (_dir, path) = temp("big.txt", body.as_bytes());
        let read = WorkspaceReader::read(
            &path,
            ReadWindow::new(Some(4_998), Some(5_000), 1024),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(read.lines.len(), 3);
        assert_eq!(read.lines[0].0, 4_998);
    }

    #[tokio::test]
    async fn clip_points_at_the_first_unseen_line() {
        let mut body = String::new();
        for i in 0..500 {
            body.push_str(&format!("{i:04}{}\n", "z".repeat(40)));
        }
        let (_dir, path) = temp("big.txt", body.as_bytes());
        let read = WorkspaceReader::read(
            &path,
            ReadWindow::new(None, None, 900),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let Clip::AtLine { next_line } = read.clip else {
            panic!("expected a line-boundary clip, got {:?}", read.clip);
        };
        assert_eq!(read.lines.last().unwrap().0 + 1, next_line);
    }

    #[tokio::test]
    async fn one_line_longer_than_the_budget_is_cut_inside_the_line() {
        let (_dir, path) = temp("long.txt", "q".repeat(10_000).as_bytes());
        let read = WorkspaceReader::read(
            &path,
            ReadWindow::new(None, None, 512),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(read.clip, Clip::InsideLine { line: 1, .. }));
        assert!(read.lines[0].1.len() < 512);
    }

    #[tokio::test]
    async fn invalid_utf8_is_an_error_not_a_replacement_character() {
        let (_dir, path) = temp("bad.txt", b"fine\nbroken \xC3\x28\n");
        let err = WorkspaceReader::read(
            &path,
            ReadWindow::new(None, None, 4096),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ReadError::NotUtf8 { line: 2 }), "{err:?}");
    }

    #[tokio::test]
    async fn nul_bytes_are_binary() {
        let (_dir, path) = temp("bin", b"MZ\x00\x00payload\n");
        let err = WorkspaceReader::read(
            &path,
            ReadWindow::new(None, None, 4096),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ReadError::Binary), "{err:?}");
    }

    #[tokio::test]
    async fn the_fingerprint_covers_the_whole_file_not_the_window() {
        let (_dir, path) = temp("a.txt", b"one\ntwo\nthree\n");
        let narrow = WorkspaceReader::read(
            &path,
            ReadWindow::new(Some(1), Some(1), 4096),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let whole = WorkspaceReader::read(
            &path,
            ReadWindow::new(None, None, 4096),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            narrow.observation.fingerprint,
            whole.observation.fingerprint
        );
        assert_eq!(
            whole.observation.fingerprint,
            leveler_context::FileStateTracker::fingerprint(b"one\ntwo\nthree\n")
        );
    }
}
