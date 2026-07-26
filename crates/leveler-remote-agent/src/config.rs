//! What a host keeps on disk to be reachable: its key, its id, and where its
//! relay is.
//!
//! Three files under `~/.leveler/remote/`, and the split between them is the
//! point:
//!
//! - `runtime_key` — the private key, `0600`. Losing it means re-pairing every
//!   device; leaking it means someone else can sign as this machine.
//! - `config.toml` — relay URL, display name, policy. Not secret, but it lives
//!   beside the key, so it gets the same restrictive mode rather than a second
//!   permissions story.
//! - `devices.json` — the keys this host accepted (owned by [`crate::TrustedDevices`]).
//!
//! The `runtime_id` is derived from the public key rather than chosen. A name
//! someone picks can collide, be typo'd, or be claimed on a relay by whoever
//! asks first; a derived one is bound to the key that has to sign for it, so
//! "this id belongs to that key" needs no registry to be true.

use std::path::{Path, PathBuf};

use leveler_remote_protocol::pairing::PairingScope;
use leveler_remote_protocol::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Default window before a remote-only approval is denied, in seconds.
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 =
    leveler_remote_protocol::policy::DEFAULT_APPROVAL_TIMEOUT_SECS;

/// `~/.leveler/remote/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Base URL of the relay this host dials out to, without a trailing slash.
    pub relay_url: String,
    /// What the phone shows for this machine.
    pub display_name: String,
    /// Derived from the runtime key; see [`runtime_id_for`].
    pub runtime_id: String,
    /// How long a remote-only approval waits before the host denies it.
    #[serde(default = "default_timeout")]
    pub approval_timeout_secs: u64,
    /// Whether a remote client may set the full-access permission profile.
    /// Off unless the user turns it on here, which is the local opt-in the
    /// capability gate requires.
    #[serde(default)]
    pub allow_full_access: bool,
    /// The scope offered when confirming a pairing, unless overridden.
    #[serde(default)]
    pub default_pair_scope: PairingScope,
}

fn default_timeout() -> u64 {
    DEFAULT_APPROVAL_TIMEOUT_SECS
}

impl RemoteConfig {
    pub fn new(
        relay_url: impl Into<String>,
        display_name: impl Into<String>,
        runtime_id: String,
    ) -> Self {
        Self {
            relay_url: relay_url.into().trim_end_matches('/').to_string(),
            display_name: display_name.into(),
            runtime_id,
            approval_timeout_secs: DEFAULT_APPROVAL_TIMEOUT_SECS,
            allow_full_access: false,
            default_pair_scope: PairingScope::Interactive,
        }
    }

    pub fn approval_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.approval_timeout_secs)
    }

    /// The relay's WebSocket base, derived from its HTTP one so a user
    /// configures one URL rather than two that could disagree.
    pub fn relay_ws_url(&self) -> String {
        match self.relay_url.split_once("://") {
            Some(("https", rest)) => format!("wss://{rest}"),
            Some(("http", rest)) => format!("ws://{rest}"),
            // Already a WebSocket scheme, or something unusual the user meant.
            _ => self.relay_url.clone(),
        }
    }
}

/// A runtime id bound to its key: `rt_` plus the first 16 hex characters of
/// `SHA-256(pubkey_raw)`.
///
/// Same construction as the pairing fingerprint, so a user comparing the two
/// sees the same digits and does not have to learn a second identifier scheme.
pub fn runtime_id_for(key: &VerifyingKey) -> String {
    format!("rt_{}", key.fingerprint())
}

/// The directory a host keeps its remote state in.
#[derive(Debug, Clone)]
pub struct RemoteHome {
    dir: PathBuf,
}

/// Why the on-disk state could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("could not parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("the runtime key at {0} is not a 32-byte Ed25519 seed")]
    MalformedKey(PathBuf),
    #[error("a runtime key already exists at {0}; refusing to replace it")]
    KeyExists(PathBuf),
}

impl RemoteHome {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.join("runtime_key")
    }

    pub fn devices_path(&self) -> PathBuf {
        self.dir.join("devices.json")
    }

    /// The stored configuration, or `None` when remote control was never
    /// enabled here.
    pub fn load_config(&self) -> Result<Option<RemoteConfig>, ConfigError> {
        let path = self.config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let config = toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })?;
        Ok(Some(config))
    }

    pub fn save_config(&self, config: &RemoteConfig) -> Result<(), ConfigError> {
        self.ensure_dir()?;
        let text = toml::to_string_pretty(config).expect("config is serializable");
        write_private(&self.config_path(), text.as_bytes())?;
        Ok(())
    }

    /// The host's signing key, or `None` when it was never created.
    pub fn load_key(&self) -> Result<Option<SigningKey>, ConfigError> {
        let path = self.key_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let seed =
            decode_seed(text.trim()).ok_or_else(|| ConfigError::MalformedKey(path.clone()))?;
        let key = SigningKey::from_seed(&seed).map_err(|_| ConfigError::MalformedKey(path))?;
        Ok(Some(key))
    }

    /// Create the host's key, refusing to overwrite one that exists.
    ///
    /// Replacing it would silently orphan every paired device — their frames
    /// would still verify against a key this host no longer has — so a second
    /// `enable` keeps the identity it already established.
    pub fn create_key(&self) -> Result<SigningKey, ConfigError> {
        let path = self.key_path();
        if path.exists() {
            return Err(ConfigError::KeyExists(path));
        }
        self.ensure_dir()?;
        let (key, seed) =
            SigningKey::generate().map_err(|_| ConfigError::MalformedKey(path.clone()))?;
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(seed);
        write_private(&path, encoded.as_bytes())?;
        Ok(key)
    }

    fn ensure_dir(&self) -> Result<(), ConfigError> {
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

fn decode_seed(text: &str) -> Option<[u8; 32]> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()?;
    raw.try_into().ok()
}

/// Write a file only the owner can read.
///
/// The mode is set before the bytes land, so the key is never briefly readable
/// by anything else on the machine.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runtime_id_is_bound_to_its_key() {
        let key = SigningKey::from_seed(&[7u8; 32]).unwrap();
        let id = runtime_id_for(&key.verifying_key());
        assert!(id.starts_with("rt_"));
        assert_eq!(id.len(), 3 + 16);
        assert!(
            leveler_remote_protocol::id_is_valid(&id),
            "{id} must be a legal envelope id"
        );
        // Same key, same id — a host that restarts keeps its identity.
        assert_eq!(id, runtime_id_for(&key.verifying_key()));
        // A different key cannot land on the same id.
        let other = SigningKey::from_seed(&[8u8; 32]).unwrap();
        assert_ne!(id, runtime_id_for(&other.verifying_key()));
    }

    #[test]
    fn a_key_round_trips_and_is_not_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        let home = RemoteHome::new(dir.path().join("remote"));
        let created = home.create_key().unwrap();
        let loaded = home.load_key().unwrap().expect("the key was just written");
        assert_eq!(created.verifying_key(), loaded.verifying_key());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(home.key_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "a private key must not be group/world readable"
            );
        }
    }

    /// Regenerating would leave every paired phone verifying against a key this
    /// host no longer has — pairings that look valid and are not.
    #[test]
    fn creating_a_second_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = RemoteHome::new(dir.path());
        home.create_key().unwrap();
        assert!(matches!(home.create_key(), Err(ConfigError::KeyExists(_))));
    }

    #[test]
    fn config_round_trips_with_defaults_for_absent_fields() {
        let dir = tempfile::tempdir().unwrap();
        let home = RemoteHome::new(dir.path());
        assert!(
            home.load_config().unwrap().is_none(),
            "nothing configured yet"
        );

        let config = RemoteConfig::new("https://relay.example/", "mbp", "rt_abc".to_string());
        assert_eq!(
            config.relay_url, "https://relay.example",
            "the trailing slash is dropped"
        );
        home.save_config(&config).unwrap();
        assert_eq!(home.load_config().unwrap().unwrap(), config);

        // A file written by an older build, missing the newer keys.
        std::fs::write(
            home.config_path(),
            "relay_url = \"https://r\"\ndisplay_name = \"x\"\nruntime_id = \"rt_1\"\n",
        )
        .unwrap();
        let loaded = home.load_config().unwrap().unwrap();
        assert_eq!(loaded.approval_timeout_secs, DEFAULT_APPROVAL_TIMEOUT_SECS);
        assert!(!loaded.allow_full_access);
        assert_eq!(loaded.default_pair_scope, PairingScope::Interactive);
    }

    #[test]
    fn the_websocket_url_follows_the_http_one() {
        let https = RemoteConfig::new("https://relay.example", "x", "rt_1".to_string());
        assert_eq!(https.relay_ws_url(), "wss://relay.example");
        let http = RemoteConfig::new("http://127.0.0.1:8443", "x", "rt_1".to_string());
        assert_eq!(http.relay_ws_url(), "ws://127.0.0.1:8443");
    }
}
