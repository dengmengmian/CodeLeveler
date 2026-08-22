//! Uploading a file from a phone.
//!
//! A signed envelope is capped well below the size of a screenshot, so an
//! upload arrives in pieces and is put back together here. Two things this
//! deliberately does *not* do:
//!
//! - **Write to the media store.** The bytes go to the runtime as an ordinary
//!   `AddAttachmentData`, the same command a local client sends, so processing,
//!   content addressing and session accounting stay in one implementation. An
//!   agent that wrote the store directly would be a second one, and the two
//!   would drift.
//! - **Answer before the runtime has.** The `AttachmentRef` a phone receives is
//!   the one the runtime produced, which is why it can be signed at all: the
//!   agent has nothing of its own to say about a file it only relayed.
//!
//! The limits are the design's: 5 MiB per file, 20 MiB per session. They are
//! enforced on the *decoded* length as chunks arrive, so a caller cannot get
//! past them by claiming a small total and then sending more.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The largest single attachment a remote device may upload.
pub const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;

/// The largest total a remote device may add to one session.
pub const MAX_SESSION_BYTES: usize = 20 * 1024 * 1024;

/// The most chunks one upload may be split into.
///
/// At the 1 MiB envelope limit, five chunks already covers the largest legal
/// attachment; the bound exists so a caller cannot pin memory by opening
/// thousands of assemblies that never complete.
pub const MAX_CHUNKS: u32 = 64;

/// Raw bytes per fetch chunk. Base64 expands ~4/3, so this stays under the
/// 1 MiB envelope cap with room for JSON wrapping.
pub const FETCH_CHUNK_BYTES: usize = 512 * 1024;

/// One `fetch_attachment` request. `chunk_index` 0 is the first piece.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchChunkRequest {
    pub sha256: String,
    #[serde(default)]
    pub chunk_index: u32,
}

/// One piece of a fetched attachment. The phone concatenates `data_base64`
/// across `chunk_total` responses. There is no public download URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchChunkResponse {
    pub sha256: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub chunk_index: u32,
    pub chunk_total: u32,
    pub data_base64: String,
}

/// Content-address ids are 64 lowercase hex characters. Anything else is
/// refused before the runtime is asked, so `../` never becomes a path.
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One piece of an upload, as it arrives inside a signed `rpc_request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadChunk {
    pub session_id: String,
    pub name: String,
    #[serde(default)]
    pub chunk_index: u32,
    #[serde(default = "one")]
    pub chunk_total: u32,
    pub data_base64: String,
}

fn one() -> u32 {
    1
}

/// Why an upload was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    #[error("attachment chunk is not valid base64")]
    NotBase64,
    #[error("chunk {index} of {total} is out of range")]
    ChunkOutOfRange { index: u32, total: u32 },
    #[error("this upload contradicts the one already in progress")]
    Contradicts,
    #[error("attachment exceeds {MAX_ATTACHMENT_BYTES} bytes")]
    TooLarge,
    #[error("session attachment total exceeds {MAX_SESSION_BYTES} bytes")]
    SessionFull,
}

impl UploadError {
    pub fn code(&self) -> &'static str {
        match self {
            UploadError::NotBase64
            | UploadError::ChunkOutOfRange { .. }
            | UploadError::Contradicts => "invalid_frame",
            UploadError::TooLarge | UploadError::SessionFull => "payload_too_large",
        }
    }
}

/// A partially received file.
#[derive(Debug)]
struct Assembly {
    total: u32,
    /// Indexed by chunk, so a repeated chunk replaces rather than appends —
    /// a retried upload must not double the file.
    pieces: Vec<Option<Vec<u8>>>,
}

impl Assembly {
    fn new(total: u32) -> Self {
        Self {
            total,
            pieces: (0..total).map(|_| None).collect(),
        }
    }

    fn received_bytes(&self) -> usize {
        self.pieces
            .iter()
            .filter_map(|piece| piece.as_ref())
            .map(Vec::len)
            .sum()
    }

    fn complete(&self) -> bool {
        self.pieces.iter().all(Option::is_some)
    }

    fn joined(&self) -> Vec<u8> {
        self.pieces
            .iter()
            .filter_map(|piece| piece.as_ref())
            .flat_map(|piece| piece.iter().copied())
            .collect()
    }
}

/// Uploads in flight, and what each session has already accepted.
#[derive(Debug, Default)]
pub struct Uploads {
    inner: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// `(device_id, session_id, name)` → the pieces so far.
    partial: HashMap<(String, String, String), Assembly>,
    /// `session_id` → bytes already handed to the runtime.
    accepted: HashMap<String, usize>,
}

/// What accepting a chunk produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkOutcome {
    /// More chunks are expected.
    Incomplete,
    /// The file is whole; these are its bytes.
    Complete(Vec<u8>),
}

impl Uploads {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one chunk, and say whether the file is now whole.
    ///
    /// The assembly is dropped as soon as it completes, so the caller owns the
    /// bytes and a second `chunk_total`-th chunk starts a new upload rather
    /// than re-delivering the old one.
    pub fn accept(
        &self,
        device_id: &str,
        chunk: &UploadChunk,
    ) -> Result<ChunkOutcome, UploadError> {
        if chunk.chunk_total == 0
            || chunk.chunk_total > MAX_CHUNKS
            || chunk.chunk_index >= chunk.chunk_total
        {
            return Err(UploadError::ChunkOutOfRange {
                index: chunk.chunk_index,
                total: chunk.chunk_total,
            });
        }

        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(chunk.data_base64.as_bytes())
            .map_err(|_| UploadError::NotBase64)?;

        let key = (
            device_id.to_string(),
            chunk.session_id.clone(),
            chunk.name.clone(),
        );
        let mut state = self.inner.lock().expect("uploads mutex");

        // Everything this session is holding: files already handed over *and*
        // pieces still in flight. Counting only the completed ones let a device
        // open one assembly per file name and sit just under the per-file cap
        // in each — the session cap said 20 MiB while the process held as much
        // as the caller cared to send.
        let already: usize = state.accepted.get(&chunk.session_id).copied().unwrap_or(0)
            + state
                .partial
                .iter()
                .filter(|((_, session, _), _)| session == &chunk.session_id)
                .map(|(_, assembly)| assembly.received_bytes())
                .sum::<usize>();
        let assembly = state
            .partial
            .entry(key.clone())
            .or_insert_with(|| Assembly::new(chunk.chunk_total));
        // A caller who changes the total mid-upload is either confused or
        // probing; either way the pieces on hand no longer describe one file.
        if assembly.total != chunk.chunk_total {
            state.partial.remove(&key);
            return Err(UploadError::Contradicts);
        }

        let assembly = state.partial.get_mut(&key).expect("just inserted");
        let previous = assembly.pieces[chunk.chunk_index as usize]
            .as_ref()
            .map(Vec::len)
            .unwrap_or(0);
        let size = assembly.received_bytes() - previous + bytes.len();
        if size > MAX_ATTACHMENT_BYTES {
            state.partial.remove(&key);
            return Err(UploadError::TooLarge);
        }
        let elsewhere = already - (assembly.received_bytes()).min(already);
        if elsewhere + size > MAX_SESSION_BYTES {
            state.partial.remove(&key);
            return Err(UploadError::SessionFull);
        }
        assembly.pieces[chunk.chunk_index as usize] = Some(bytes);

        if !assembly.complete() {
            return Ok(ChunkOutcome::Incomplete);
        }
        let assembly = state.partial.remove(&key).expect("just checked");
        let joined = assembly.joined();
        *state.accepted.entry(chunk.session_id.clone()).or_insert(0) += joined.len();
        Ok(ChunkOutcome::Complete(joined))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn encode(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn chunk(index: u32, total: u32, bytes: &[u8]) -> UploadChunk {
        UploadChunk {
            session_id: "s1".to_string(),
            name: "shot.png".to_string(),
            chunk_index: index,
            chunk_total: total,
            data_base64: encode(bytes),
        }
    }

    #[test]
    fn pieces_are_joined_in_index_order_not_arrival_order() {
        let uploads = Uploads::new();
        // Out of order on purpose: chunks travel as separate RPCs and nothing
        // guarantees the order they are served in.
        assert_eq!(
            uploads.accept("dev", &chunk(1, 2, b"world")).unwrap(),
            ChunkOutcome::Incomplete
        );
        assert_eq!(
            uploads.accept("dev", &chunk(0, 2, b"hello ")).unwrap(),
            ChunkOutcome::Complete(b"hello world".to_vec())
        );
    }

    #[test]
    fn a_repeated_chunk_replaces_rather_than_doubles() {
        let uploads = Uploads::new();
        uploads.accept("dev", &chunk(0, 2, b"hello ")).unwrap();
        // A retry of the same chunk — the phone resends when an answer is lost.
        uploads.accept("dev", &chunk(0, 2, b"hello ")).unwrap();
        assert_eq!(
            uploads.accept("dev", &chunk(1, 2, b"world")).unwrap(),
            ChunkOutcome::Complete(b"hello world".to_vec())
        );
    }

    #[test]
    fn two_devices_uploading_the_same_name_do_not_share_pieces() {
        let uploads = Uploads::new();
        uploads.accept("alice", &chunk(0, 2, b"AAAA")).unwrap();
        // Bob completing his own upload must not be completed by Alice's piece.
        assert_eq!(
            uploads.accept("bob", &chunk(0, 2, b"BBBB")).unwrap(),
            ChunkOutcome::Incomplete
        );
        assert_eq!(
            uploads.accept("bob", &chunk(1, 2, b"bbbb")).unwrap(),
            ChunkOutcome::Complete(b"BBBBbbbb".to_vec())
        );
    }

    #[test]
    fn the_size_cap_counts_decoded_bytes_as_they_arrive() {
        let uploads = Uploads::new();
        let big = vec![0u8; MAX_ATTACHMENT_BYTES / 2 + 1];
        uploads.accept("dev", &chunk(0, 2, &big)).unwrap();
        // Claiming a small total buys nothing: the second half is what pushes
        // it over, and it is measured after decoding.
        assert_eq!(
            uploads.accept("dev", &chunk(1, 2, &big)),
            Err(UploadError::TooLarge)
        );
        // And the half-file is gone rather than left pinning memory.
        assert_eq!(
            uploads.accept("dev", &chunk(0, 1, b"small")).unwrap(),
            ChunkOutcome::Complete(b"small".to_vec())
        );
    }

    #[test]
    fn a_session_total_is_enforced_across_separate_files() {
        let uploads = Uploads::new();
        let file = vec![0u8; MAX_ATTACHMENT_BYTES];
        for index in 0..4 {
            let mut one = chunk(0, 1, &file);
            one.name = format!("file{index}.bin");
            assert!(matches!(
                uploads.accept("dev", &one).unwrap(),
                ChunkOutcome::Complete(_)
            ));
        }
        let mut over = chunk(0, 1, b"one byte too many");
        over.name = "file4.bin".to_string();
        assert_eq!(uploads.accept("dev", &over), Err(UploadError::SessionFull));
    }

    #[test]
    fn half_finished_uploads_count_against_the_session_too() {
        let uploads = Uploads::new();
        let half = vec![0u8; MAX_ATTACHMENT_BYTES / 2];
        // Eight files, each left deliberately incomplete at half the per-file
        // cap: 20 MiB held, none of it finished. Counting only *completed*
        // files against the session total left that counter reading zero, so
        // the ninth — and the ninetieth — would have been accepted too.
        for index in 0..8 {
            let mut piece = chunk(0, 2, &half);
            piece.name = format!("file{index}.bin");
            assert_eq!(
                uploads.accept("dev", &piece),
                Ok(ChunkOutcome::Incomplete),
                "file{index} is still inside the session cap"
            );
        }
        let mut one_more = chunk(0, 2, &half);
        one_more.name = "file8.bin".to_string();
        assert_eq!(
            uploads.accept("dev", &one_more),
            Err(UploadError::SessionFull),
            "in-flight pieces must count against the session cap"
        );
    }

    #[test]
    fn nonsense_chunk_counts_are_refused_before_any_allocation() {
        let uploads = Uploads::new();
        assert!(matches!(
            uploads.accept("dev", &chunk(0, 0, b"x")),
            Err(UploadError::ChunkOutOfRange { .. })
        ));
        assert!(matches!(
            uploads.accept("dev", &chunk(3, 2, b"x")),
            Err(UploadError::ChunkOutOfRange { .. })
        ));
        assert!(matches!(
            uploads.accept("dev", &chunk(0, MAX_CHUNKS + 1, b"x")),
            Err(UploadError::ChunkOutOfRange { .. })
        ));
    }

    #[test]
    fn a_changed_total_mid_upload_discards_what_was_collected() {
        let uploads = Uploads::new();
        uploads.accept("dev", &chunk(0, 3, b"aaa")).unwrap();
        assert_eq!(
            uploads.accept("dev", &chunk(1, 2, b"bbb")),
            Err(UploadError::Contradicts)
        );
        // The contradictory upload took the old pieces with it, so this starts
        // clean rather than completing a file made of two different attempts.
        assert_eq!(
            uploads.accept("dev", &chunk(0, 1, b"ccc")).unwrap(),
            ChunkOutcome::Complete(b"ccc".to_vec())
        );
    }

    #[test]
    fn bad_base64_is_refused() {
        let uploads = Uploads::new();
        let mut bad = chunk(0, 1, b"x");
        bad.data_base64 = "!!!not base64!!!".to_string();
        assert_eq!(uploads.accept("dev", &bad), Err(UploadError::NotBase64));
    }
}
