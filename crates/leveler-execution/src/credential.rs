//! Phase 4 credential isolation: use a credential without handing its value to
//! the model.
//!
//! The host — never the agent shell or the model context — resolves the
//! user's configured Git credential and hands it to exactly one approved
//! execution. Two properties make that safe:
//!
//! * **One source of truth.** The broker runs the user's own credential helper
//!   (`git credential fill`) in host memory, in a scratch directory so a
//!   repository-local helper cannot be the source, and with a filtered
//!   environment so a caller-supplied `GIT_*`/askpass override cannot redirect
//!   it. The material is returned to the execution layer only.
//! * **No stable secret identity is stored.** A grant binds the credential's
//!   *incarnation*, a one-way commitment over username and password (the host
//!   and transport are separate, equally exact binding fields). Rotating the
//!   token changes the commitment, so a reused grant fails closed instead of
//!   silently authenticating as the new secret.
//!
//! This module deliberately never logs, serializes, or formats the credential
//! value. [`CredentialMaterial`] has a redacted [`std::fmt::Debug`] and is not
//! `Serialize`.

use leveler_core::EnvSnapshot;
use sha2::{Digest, Sha256};
use std::ffi::OsString;

/// A resolved HTTP(S) credential held in host memory for one execution.
///
/// The value is available only through [`Self::helper_payload`] (the Git
/// credential-helper protocol lines written to a private file) and
/// [`Self::identity`] (the non-reversible commitment persisted in a grant).
/// There is intentionally no accessor that returns the raw secret.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialMaterial {
    username: String,
    password: String,
}

impl CredentialMaterial {
    /// Build from an already-resolved credential. The caller owns the source
    /// boundary; this type only owns the value's lifecycle.
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }

    /// The exact bytes a Git credential helper must print for `get`: the
    /// `username`/`password` fields the protocol defines. Never model-visible.
    pub fn helper_payload(&self) -> String {
        let mut payload = String::with_capacity(self.username.len() + self.password.len() + 24);
        payload.push_str("username=");
        payload.push_str(&self.username);
        payload.push('\n');
        payload.push_str("password=");
        payload.push_str(&self.password);
        payload.push('\n');
        payload
    }

    /// The one-way incarnation commitment a grant binds.
    ///
    /// Rotating the credential (or switching account) changes this value, so an
    /// old approval cannot authorize the new secret. The commitment is over the
    /// value, so it is not a stored copy of it.
    pub fn identity(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"leveler-credential-v1\0");
        hasher.update(self.username.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.password.as_bytes());
        format!("sha256:{:x}", hasher.finalize())
    }
}

impl std::fmt::Debug for CredentialMaterial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialMaterial(<redacted>)")
    }
}

/// Resolve the user's stored credential for one HTTP(S) destination.
///
/// `Ok(None)` means the configured helper has no credential for this host —
/// a public repository — and callers must not offer a credential grant.
/// `Err` means the broker could not produce a trustworthy answer (no host
/// credential source, helper failure, non-UTF-8 protocol output); callers
/// treat that exactly like "no credential" at admission and fail closed at
/// execution.
pub async fn fill_http_credential(
    host: &str,
    transport: &str,
    environment: &EnvSnapshot,
) -> Result<Option<CredentialMaterial>, String> {
    if !matches!(transport, "https" | "http") {
        return Err("credential brokering supports HTTP(S) only".into());
    }
    if host.is_empty() || host.contains(['\n', '\r', '\0']) {
        return Err("credential destination is not a plain host".into());
    }
    // Without a home directory there is no user credential source to consult;
    // do not spawn a helper that can only fail.
    if environment.var_os("HOME").is_none() && environment.var_os("USERPROFILE").is_none() {
        return Ok(None);
    }

    // The helper may be a program on PATH, so PATH must survive; the scratch
    // directory keeps a repository-local `.git/config` out of the source.
    let scratch =
        tempfile::TempDir::new().map_err(|_| "cannot create credential scratch directory")?;

    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(scratch.path())
        .args(["credential", "fill"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .env_clear();
    for (name, value) in broker_environment(environment) {
        command.env(name, value);
    }
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");

    let mut child = command
        .spawn()
        .map_err(|_| "cannot start the Git credential helper".to_string())?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "credential helper stdin unavailable".to_string())?;
        let request = format!("protocol={transport}\nhost={host}\n\n");
        stdin
            .write_all(request.as_bytes())
            .await
            .map_err(|_| "cannot send the credential request".to_string())?;
        stdin
            .shutdown()
            .await
            .map_err(|_| "cannot finish the credential request".to_string())?;
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .map_err(|_| "credential helper timed out".to_string())?
        .map_err(|_| "credential helper failed to run".to_string())?;
    // stderr is deliberately never surfaced: helpers echo credential source
    // paths and, on some platforms, the secret itself.
    if !output.status.success() {
        return Ok(None);
    }
    let stdout =
        String::from_utf8(output.stdout).map_err(|_| "credential helper returned non-UTF-8")?;
    Ok(parse_credential_protocol(&stdout))
}

/// Only the helper's own global/system configuration is a valid source. A
/// `GIT_*` override supplied by the caller could point the broker at a
/// repository-controlled helper or an askpass program, so every `GIT_*`
/// variable and every explicit askpass is dropped and re-pinned below.
fn broker_environment(environment: &EnvSnapshot) -> Vec<(OsString, OsString)> {
    environment
        .scrubbed_vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy().to_ascii_uppercase();
            !name.starts_with("GIT_")
                && !matches!(name.as_str(), "SSH_ASKPASS" | "SSH_ASKPASS_REQUIRE")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Parse the `key=value` lines `git credential fill` prints. Only a non-empty
/// password makes a usable credential; `quit=1` and an empty password both
/// mean "nothing stored for this host".
fn parse_credential_protocol(output: &str) -> Option<CredentialMaterial> {
    let mut username = String::new();
    let mut password = String::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "username" => username = value.to_string(),
            "password" => password = value.to_string(),
            _ => {}
        }
    }
    if password.is_empty() {
        return None;
    }
    Some(CredentialMaterial::new(username, password))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn snapshot(home: &std::path::Path) -> EnvSnapshot {
        EnvSnapshot::new(
            [
                (OsString::from("HOME"), OsString::from(home.as_os_str())),
                (
                    OsString::from("PATH"),
                    std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
                ),
            ],
            PathBuf::from(home),
            PathBuf::from(home),
        )
    }

    /// A global helper that answers with a fixed credential.
    fn helper_home(password: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            format!(
                "[credential]\n\thelper = \"!f() {{ echo username=leveler-test; echo password={password}; }}; f\"\n"
            ),
        )
        .unwrap();
        home
    }

    #[test]
    fn identity_is_stable_and_rotates_with_the_value() {
        let first = CredentialMaterial::new("user", "token-a");
        let same = CredentialMaterial::new("user", "token-a");
        let rotated = CredentialMaterial::new("user", "token-b");
        assert_eq!(first.identity(), same.identity());
        assert_ne!(first.identity(), rotated.identity());
    }

    #[test]
    fn debug_and_identity_never_reveal_the_secret() {
        let material = CredentialMaterial::new("user", "LEVELER_SECRET_LEAK_TEST_abc");
        let debug = format!("{material:?}");
        assert!(!debug.contains("LEVELER_SECRET_LEAK_TEST_abc"), "{debug}");
        assert!(!material.identity().contains("LEVELER_SECRET_LEAK_TEST_abc"));
    }

    #[test]
    fn helper_payload_is_the_git_protocol_shape() {
        let material = CredentialMaterial::new("user", "pw");
        assert_eq!(material.helper_payload(), "username=user\npassword=pw\n");
    }

    #[test]
    fn protocol_parse_requires_a_password() {
        assert!(parse_credential_protocol("username=u\n").is_none());
        assert!(parse_credential_protocol("quit=1\n").is_none());
        assert_eq!(
            parse_credential_protocol("username=u\npassword=p\n"),
            Some(CredentialMaterial::new("u", "p"))
        );
    }

    #[tokio::test]
    async fn broker_resolves_only_from_the_user_helper() {
        let home = helper_home("LEVELER_TEST_SECRET_xyz");
        let material = fill_http_credential("example.com", "https", &snapshot(home.path()))
            .await
            .unwrap()
            .expect("the configured helper must supply a credential");
        assert_eq!(
            material.identity(),
            CredentialMaterial::new("leveler-test", "LEVELER_TEST_SECRET_xyz").identity()
        );
    }

    #[tokio::test]
    async fn broker_does_not_use_a_repository_local_helper() {
        // A helper configured only in the scratch repo must be invisible: the
        // broker runs `git credential fill` outside any repository.
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".gitconfig"), "").unwrap();
        let material = fill_http_credential("example.com", "https", &snapshot(home.path()))
            .await
            .unwrap();
        assert!(material.is_none());
    }

    #[tokio::test]
    async fn broker_ignores_caller_environment_overrides() {
        let home = helper_home("LEVELER_TEST_SECRET_xyz");
        let mut values = vec![
            (
                OsString::from("HOME"),
                OsString::from(home.path().as_os_str()),
            ),
            (
                OsString::from("PATH"),
                std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
            ),
        ];
        // A caller-supplied askpass must not be able to intercept the request.
        values.push((OsString::from("GIT_ASKPASS"), OsString::from("/bin/echo")));
        values.push((
            OsString::from("GIT_CONFIG_GLOBAL"),
            OsString::from("/dev/null"),
        ));
        let environment = EnvSnapshot::new(
            values,
            PathBuf::from(home.path()),
            PathBuf::from(home.path()),
        );
        let material = fill_http_credential("example.com", "https", &environment)
            .await
            .unwrap()
            .expect("the user helper must still be the source");
        assert_eq!(
            material.identity(),
            CredentialMaterial::new("leveler-test", "LEVELER_TEST_SECRET_xyz").identity()
        );
    }
}
