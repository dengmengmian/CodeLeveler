//! Immutable host capabilities captured by the application composition root.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default)]
pub struct EnvSnapshot {
    values: BTreeMap<OsString, OsString>,
    current_dir: PathBuf,
    temp_dir: PathBuf,
}

impl EnvSnapshot {
    pub fn new(
        values: impl IntoIterator<Item = (OsString, OsString)>,
        current_dir: PathBuf,
        temp_dir: PathBuf,
    ) -> Self {
        Self {
            values: values.into_iter().collect(),
            current_dir,
            temp_dir,
        }
    }

    pub fn var_os(&self, key: impl AsRef<OsStr>) -> Option<OsString> {
        self.values.get(key.as_ref()).cloned()
    }

    pub fn var(&self, key: impl AsRef<OsStr>) -> Option<String> {
        self.var_os(key).and_then(|v| v.into_string().ok())
    }

    pub fn vars_os(&self) -> impl Iterator<Item = (&OsString, &OsString)> {
        self.values.iter()
    }
    pub fn current_dir(&self) -> &Path {
        &self.current_dir
    }
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir
    }
    pub fn paths(&self, key: impl AsRef<OsStr>) -> Vec<PathBuf> {
        self.var_os(key)
            .map(|v| std::env::split_paths(&v).collect())
            .unwrap_or_default()
    }
}

static ENVIRONMENT: OnceLock<EnvSnapshot> = OnceLock::new();

pub fn install_environment(snapshot: EnvSnapshot) -> Result<(), EnvSnapshot> {
    ENVIRONMENT.set(snapshot)
}

/// The installed process capabilities. Library-only callers get an empty,
/// deterministic snapshot and must install/pass capabilities to use host state.
pub fn environment() -> &'static EnvSnapshot {
    static EMPTY: OnceLock<EnvSnapshot> = OnceLock::new();
    ENVIRONMENT
        .get()
        .unwrap_or_else(|| EMPTY.get_or_init(EnvSnapshot::default))
}
