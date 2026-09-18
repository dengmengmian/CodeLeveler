//! Update failures, as product copy a human can act on.
//!
//! Every variant names the thing that failed. None of them is a generic
//! "update failed": a checksum mismatch and a missing asset need different
//! actions, and the CLI's exit code and the TUI's line both say which happened.

/// What went wrong during check/download/verify/install.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("network request failed: {0}")]
    Network(String),
    #[error("GitHub returned HTTP {status} for {url}")]
    Http { status: u16, url: String },
    #[error("no GitHub release found for {0}")]
    NoRelease(String),
    #[error("no stable GitHub release found for {repo}")]
    NoStableRelease { repo: String },
    #[error("release tag `{0}` is not a semantic version")]
    UnparseableTag(String),
    #[error("no prebuilt binary for this host ({triple}) in release {tag}")]
    NoAsset { triple: String, tag: String },
    #[error("release {tag} ships no {asset}.sha256; refusing to install unverified bytes")]
    NoChecksum { tag: String, asset: String },
    #[error("checksum file for {asset} is malformed")]
    MalformedChecksum { asset: String },
    #[error("checksum verification failed for {asset}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
    #[error("could not unpack {asset}: {reason}")]
    Extract { asset: String, reason: String },
    #[error("archive {asset} does not contain a `{binary}` executable")]
    MissingBinary { asset: String, binary: String },
    #[error("unable to update CodeLeveler: installation directory `{dir}` is not writable")]
    NotWritable { dir: String },
    #[error("the downloaded binary failed validation: {0}")]
    Validation(String),
    #[error("self-update is unavailable for this installation: {0}")]
    Unsupported(String),
    #[error("io error: {0}")]
    Io(String),
}

impl From<std::io::Error> for UpdateError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            Self::NotWritable { dir: String::new() }
        } else {
            Self::Io(error.to_string())
        }
    }
}

impl UpdateError {
    /// Whether the failure is transient connectivity (worth retrying later)
    /// rather than a permanent condition (bad asset, bad checksum).
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Network(_) | Self::Http { .. } | Self::Io(_))
    }
}
