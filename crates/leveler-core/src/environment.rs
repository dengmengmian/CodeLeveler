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

    /// Read a variable using Windows' case-insensitive environment semantics.
    /// Useful for immutable snapshots captured on Windows, where the host may
    /// expose `Path` rather than `PATH`.
    pub fn var_os_case_insensitive(&self, key: &str) -> Option<OsString> {
        self.values.iter().find_map(|(name, value)| {
            name.to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(key))
                .then(|| value.clone())
        })
    }

    pub fn paths_case_insensitive(&self, key: &str) -> Vec<PathBuf> {
        self.var_os_case_insensitive(key)
            .map(|value| std::env::split_paths(&value).collect())
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

/// The global CodeLeveler home directory — `$LEVELER_HOME`, else `$HOME/.leveler`,
/// else `%USERPROFILE%\.leveler` (Windows). `None` when no home is known.
///
/// The single source of this resolution order. Callers pass their own env
/// lookup so each keeps its source (live `std::env` vs the installed snapshot);
/// only the order — including the `USERPROFILE` fallback — lives here, so the
/// surfaces cannot drift apart (which previously dropped `USERPROFILE` on
/// Windows in some places but not others).
pub fn leveler_home_dir_from<F>(var_os: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<OsString>,
{
    if let Some(h) = var_os("LEVELER_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(h));
    }
    var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(|h| PathBuf::from(h).join(".leveler"))
}

/// [`leveler_home_dir_from`] resolved against an [`EnvSnapshot`].
pub fn leveler_home_dir(env: &EnvSnapshot) -> Option<PathBuf> {
    leveler_home_dir_from(|k| env.var_os(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_can_read_windows_environment_names_case_insensitively() {
        let snapshot = EnvSnapshot::new(
            [(OsString::from("Path"), OsString::from("tool-bin"))],
            PathBuf::new(),
            PathBuf::new(),
        );

        assert_eq!(
            snapshot.var_os_case_insensitive("PATH"),
            Some(OsString::from("tool-bin"))
        );
    }

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| OsString::from(*v))
        }
    }

    #[test]
    fn resolution_order_prefers_leveler_home_then_home_then_userprofile() {
        assert_eq!(
            leveler_home_dir_from(lookup(&[("LEVELER_HOME", "/lh"), ("HOME", "/h")])),
            Some(PathBuf::from("/lh"))
        );
        assert_eq!(
            leveler_home_dir_from(lookup(&[("HOME", "/h"), ("USERPROFILE", "/u")])),
            Some(PathBuf::from("/h/.leveler"))
        );
        // USERPROFILE is the Windows fallback that used to be dropped.
        assert_eq!(
            leveler_home_dir_from(lookup(&[("USERPROFILE", "/u")])),
            Some(PathBuf::from("/u/.leveler"))
        );
        assert_eq!(leveler_home_dir_from(lookup(&[])), None);
        // Empty values are skipped, not treated as a set home.
        assert_eq!(
            leveler_home_dir_from(lookup(&[("LEVELER_HOME", ""), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.leveler"))
        );
    }
}
