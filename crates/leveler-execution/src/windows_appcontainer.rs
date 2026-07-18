//! Windows AppContainer FS backends (WS3-A ReadOnly + WS3-B write-restricted).
//!
//! Both axes use AppContainer isolation via the audited `rappct` crate (no
//! in-crate `unsafe`). WorkspaceWrite grants the package SID write access only
//! on host-trusted write roots — siblings stay outside the package allowlist
//! (doctor reports allowlist read + write-restricted, never full FS).
//!
//! Non-Windows builds export stubs that fail closed for restricted intents.

#[cfg(any(windows, test))]
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::command::{ProcessError, ProcessOutput, ProcessRequest};
use crate::windows_sandbox::FilesystemIntent;

#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use crate::windows_acl::AclCoordinator;
#[cfg(windows)]
use crate::windows_sandbox::validate_acl_root;

#[cfg(windows)]
const PROFILE_PREFIX: &str = "CodeLeveler.Agent";

/// Run under AppContainer according to host-trusted `intent`.
pub async fn run_appcontainer(
    request: ProcessRequest,
    intent: FilesystemIntent,
    program: &str,
    args: &[String],
    cancellation: CancellationToken,
    environment: std::sync::Arc<leveler_core::EnvSnapshot>,
) -> Result<ProcessOutput, ProcessError> {
    if cancellation.is_cancelled() {
        return Err(ProcessError::Cancelled);
    }
    #[cfg(not(windows))]
    {
        let _ = (request, intent, program, args, environment);
        Err(ProcessError::SandboxPolicy(
            "AppContainer FS backends are Windows-only".into(),
        ))
    }
    #[cfg(windows)]
    {
        run_appcontainer_windows(request, intent, program, args, cancellation, environment).await
    }
}

#[cfg(windows)]
async fn run_appcontainer_windows(
    request: ProcessRequest,
    intent: FilesystemIntent,
    program: &str,
    args: &[String],
    cancellation: CancellationToken,
    environment: std::sync::Arc<leveler_core::EnvSnapshot>,
) -> Result<ProcessOutput, ProcessError> {
    use rappct::launch::{JobLimits, LaunchOptions, StdioConfig, launch_in_container_with_io};
    use rappct::{AppContainerProfile, KnownCapability, SecurityCapabilitiesBuilder};
    use std::io::Read;

    let plan = plan_for_intent(&intent, &request, environment.temp_dir())?;
    let acl_roots = acl_roots_for_plan(&plan);
    for r in &acl_roots {
        validate_acl_root(r).map_err(ProcessError::SandboxPolicy)?;
    }

    let allow_network = !request.deny_network;
    let profile_name = profile_name_for(&request);
    let program_owned = program.to_string();
    let args_owned = args.to_vec();
    let cwd = request.cwd.clone();
    let timeout = request.timeout;
    let max_output = request.max_output_bytes;
    let deny_env = request.deny_env.clone();
    let allow_env = request.allow_env.clone();

    let join = tokio::task::spawn_blocking(move || -> Result<ProcessOutput, ProcessError> {
        // Created before `acl` so unwinding/early returns restore ACLs first,
        // then remove the command-private temp directory.
        let _sandbox_temp_guard = SandboxTempGuard(plan.sandbox_temp.clone());
        let mut acl = AclCoordinator::new();
        // A write root is also readable and therefore appears in both plan
        // lists. Snapshot/lock every physical root exactly once; preparing the
        // same root twice would make the coordinator conflict with itself.
        for r in &acl_roots {
            acl.prepare_root(r)
                .map_err(|e| ProcessError::SandboxPolicy(format!("ACL prepare: {e}")))?;
        }

        let profile = AppContainerProfile::ensure(
            &profile_name,
            "CodeLeveler agent sandbox",
            Some("Workspace isolation for tool processes"),
        )
        .map_err(|e| ProcessError::SandboxPolicy(format!("AppContainer profile: {e}")))?;

        // RO roots: RX (read + traverse/execute) so pre-existing children are
        // listable/openable; never GENERIC_ALL. Inheritance alone does not always
        // cover existing files — walk and grant each path.
        for r in &plan.read_roots {
            grant_rx_tree(r, &profile.sid).map_err(|e| {
                ProcessError::SandboxPolicy(format!("grant RX tree {}: {e}", r.display()))
            })?;
        }
        for r in &plan.write_roots {
            grant_rw_tree(r, &profile.sid).map_err(|e| {
                ProcessError::SandboxPolicy(format!("grant RW tree {}: {e}", r.display()))
            })?;
        }

        let mut cap_builder = SecurityCapabilitiesBuilder::new(&profile.sid);
        if allow_network {
            cap_builder = cap_builder.with_known(&[KnownCapability::InternetClient]);
        }
        let caps = cap_builder
            .build()
            .map_err(|e| ProcessError::SandboxPolicy(format!("security capabilities: {e}")))?;

        let cmdline = build_cmdline(&program_owned, &args_owned);
        // Inherit host env (scrubbed), then apply plan TEMP overrides so GetTempPath
        // lands inside the granted private sandbox_temp — not host TEMP.
        let mut env_pairs: Vec<(std::ffi::OsString, std::ffi::OsString)> = environment
            .vars_os()
            .filter(|(k, _)| {
                let name = k.to_string_lossy();
                if crate::command::is_credential_env_name(&name) {
                    return allow_env.iter().any(|a| a == &name);
                }
                if deny_env.iter().any(|d| d == &name) {
                    return false;
                }
                // Drop host temp vars; plan.env_overrides re-set them.
                !is_temp_env_key(&name)
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        env_pairs.extend(plan.env_overrides);
        env_pairs.push(("NO_COLOR".into(), "1".into()));
        env_pairs.push(("FORCE_COLOR".into(), "0".into()));

        let opts = LaunchOptions {
            exe: PathBuf::from(&program_owned),
            cmdline: Some(cmdline),
            cwd: Some(cwd),
            env: Some(env_pairs),
            stdio: StdioConfig::Pipe,
            suspended: false,
            join_job: Some(JobLimits {
                memory_bytes: None,
                cpu_rate_percent: None,
                kill_on_job_close: true,
            }),
            startup_timeout: Some(Duration::from_secs(30)),
        };

        let mut launched = launch_in_container_with_io(&caps, &opts).map_err(|e| {
            let _ = acl.restore_all();
            ProcessError::SandboxPolicy(format!("AppContainer launch failed: {e}"))
        })?;

        // Drain pipes on helper threads while we wait (wait consumes LaunchedIo).
        let mut stdout_file = launched.stdout.take();
        let mut stderr_file = launched.stderr.take();
        let stdout_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(ref mut f) = stdout_file {
                let _ = f.read_to_end(&mut buf);
            }
            buf
        });
        let stderr_handle = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(ref mut f) = stderr_file {
                let _ = f.read_to_end(&mut buf);
            }
            buf
        });

        let timed_out;
        let exit_code = match launched.wait(Some(timeout)) {
            Ok(code) => {
                timed_out = false;
                Some(code as i32)
            }
            Err(e) => {
                // Timeout or wait failure: dropping job_guard kills the tree.
                let msg = e.to_string();
                timed_out = msg.contains("timeout") || msg.contains("TimedOut");
                if !timed_out {
                    let _ = acl.restore_all();
                    return Err(ProcessError::Io {
                        program: program_owned.clone(),
                        source: std::io::Error::other(msg),
                    });
                }
                None
            }
        };

        let stdout_raw = stdout_handle.join().unwrap_or_default();
        let stderr_raw = stderr_handle.join().unwrap_or_default();
        let (stdout_s, dropped_o) = cap_bytes(stdout_raw, max_output);
        let (stderr_s, dropped_e) = cap_bytes(stderr_raw, max_output);
        let dropped_bytes = dropped_o + dropped_e;

        acl.restore_all()
            .map_err(|e| ProcessError::SandboxPolicy(format!("ACL restore: {e}")))?;

        Ok(ProcessOutput {
            exit_code,
            stdout: stdout_s,
            stderr: stderr_s,
            timed_out,
            truncated: dropped_bytes > 0,
            dropped_bytes,
        })
    });

    tokio::select! {
        res = join => {
            res.map_err(|e| ProcessError::Io {
                program: program.to_string(),
                source: std::io::Error::other(format!("appcontainer worker join: {e}")),
            })?
        }
        _ = cancellation.cancelled() => Err(ProcessError::Cancelled),
    }
}

/// FILE_GENERIC_EXECUTE / FILE_TRAVERSE (Win32). Combined with FILE_GENERIC_READ
/// gives directory RX without write.
#[cfg(windows)]
const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;

#[cfg(windows)]
fn access_mask_rx() -> rappct::acl::AccessMask {
    use rappct::acl::AccessMask;
    AccessMask(AccessMask::FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE)
}

/// Grant RX (read + traverse) on a directory and every pre-existing child.
/// Does not include write bits.
#[cfg(windows)]
fn grant_rx_tree(root: &std::path::Path, sid: &rappct::AppContainerSid) -> Result<(), String> {
    use rappct::acl::{ResourcePath, grant_to_package};
    let rx = access_mask_rx();
    grant_to_package(ResourcePath::Directory(root.to_path_buf()), sid, rx)
        .map_err(|e| e.to_string())?;
    walk_grant(root, sid, rx)
}

/// Grant full package access on a write root and pre-existing children.
#[cfg(windows)]
fn grant_rw_tree(root: &std::path::Path, sid: &rappct::AppContainerSid) -> Result<(), String> {
    use rappct::acl::{AccessMask, ResourcePath, grant_to_package};
    grant_to_package(
        ResourcePath::Directory(root.to_path_buf()),
        sid,
        AccessMask::GENERIC_ALL,
    )
    .map_err(|e| e.to_string())?;
    walk_grant(root, sid, AccessMask::GENERIC_ALL)
}

#[cfg(windows)]
fn walk_grant(
    root: &std::path::Path,
    sid: &rappct::AppContainerSid,
    access: rappct::acl::AccessMask,
) -> Result<(), String> {
    use rappct::acl::{ResourcePath, grant_to_package};
    let entries = std::fs::read_dir(root).map_err(|e| e.to_string())?;
    for ent in entries.flatten() {
        let path = ent.path();
        let ft = ent.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            grant_to_package(ResourcePath::Directory(path.clone()), sid, access)
                .map_err(|e| e.to_string())?;
            walk_grant(&path, sid, access)?;
        } else if ft.is_file() {
            grant_to_package(ResourcePath::File(path), sid, access).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// FS + env plan for one AppContainer spawn. Single source of truth: granted
/// roots and child `TEMP`/`TMP`/`TMPDIR` must name the same private sandbox temp.
#[derive(Debug, Clone)]
#[cfg(any(windows, test))]
struct AppContainerFsPlan {
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
    /// Private per-command temp (never the whole system temp tree).
    sandbox_temp: PathBuf,
    /// Env overrides applied at launch so GetTempPath/%TEMP% lands in `sandbox_temp`.
    env_overrides: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

#[cfg(any(windows, test))]
fn acl_roots_for_plan(plan: &AppContainerFsPlan) -> Vec<PathBuf> {
    dedup_paths(
        plan.read_roots
            .iter()
            .chain(plan.write_roots.iter())
            .cloned()
            .collect(),
    )
}

#[cfg(windows)]
struct SandboxTempGuard(PathBuf);

#[cfg(windows)]
impl Drop for SandboxTempGuard {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                %error,
                path = %self.0.display(),
                "failed to remove AppContainer private temp"
            );
        }
    }
}

#[cfg(windows)]
fn is_temp_env_key(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "TEMP" | "TMP" | "TMPDIR"
    )
}

/// Private process temp under the system temp dir — a unique subdirectory, never
/// the whole `temp_dir()` tree (which would authorize sibling canaries).
#[cfg(any(windows, test))]
fn private_sandbox_temp(
    tag: &str,
    seed: &std::path::Path,
    temp_root: &std::path::Path,
) -> Result<PathBuf, ProcessError> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let mut h = DefaultHasher::new();
    tag.hash(&mut h);
    seed.hash(&mut h);
    let p = temp_root.join(format!(
        "leveler-ac-{tag}-{:x}-{}-{}",
        h.finish(),
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&p).map_err(|error| {
        ProcessError::SandboxPolicy(format!(
            "create private AppContainer temp {}: {error}",
            p.display()
        ))
    })?;
    Ok(p)
}

#[cfg(any(windows, test))]
fn temp_env_overrides(
    sandbox_temp: &std::path::Path,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let s = sandbox_temp.as_os_str().to_os_string();
    vec![
        ("TEMP".into(), s.clone()),
        ("TMP".into(), s.clone()),
        ("TMPDIR".into(), s),
    ]
}

/// Build the full AppContainer FS plan (roots + private temp + TEMP env).
/// Host-agnostic so unit tests prove the shipped contract without Windows APIs.
#[cfg(any(windows, test))]
fn plan_for_intent(
    intent: &FilesystemIntent,
    request: &ProcessRequest,
    temp_root: &std::path::Path,
) -> Result<AppContainerFsPlan, ProcessError> {
    match intent {
        FilesystemIntent::Unrestricted => Err(ProcessError::SandboxPolicy(
            "AppContainer path is not used for Unrestricted intent".into(),
        )),
        FilesystemIntent::ReadOnly { read_roots } => {
            let mut reads = read_roots.clone();
            if reads.is_empty() {
                if let Some(w) = &request.write_root {
                    reads.push(w.clone());
                }
                reads.push(request.cwd.clone());
            }
            let seed = reads
                .first()
                .cloned()
                .unwrap_or_else(|| request.cwd.clone());
            let sandbox_temp = private_sandbox_temp("ro", &seed, temp_root)?;
            // Workspace stays RX-only (not in write_roots). Private temp is the
            // sole writeable path so GetTempPath/%TEMP% works without opening
            // workspace writes.
            reads.push(sandbox_temp.clone());
            Ok(AppContainerFsPlan {
                read_roots: dedup_paths(reads),
                write_roots: dedup_paths(vec![sandbox_temp.clone()]),
                sandbox_temp: sandbox_temp.clone(),
                env_overrides: temp_env_overrides(&sandbox_temp),
            })
        }
        FilesystemIntent::WorkspaceWrite {
            write_root,
            extra_read_roots,
        } => {
            let sandbox_temp = private_sandbox_temp("ww", write_root, temp_root)?;
            let mut writes = vec![write_root.clone(), sandbox_temp.clone()];
            let cache = temp_root.join("leveler-tool-cache");
            let _ = std::fs::create_dir_all(&cache);
            writes.push(cache);

            let mut reads = extra_read_roots.clone();
            reads.extend(writes.iter().cloned());
            Ok(AppContainerFsPlan {
                read_roots: dedup_paths(reads),
                write_roots: dedup_paths(writes),
                sandbox_temp: sandbox_temp.clone(),
                env_overrides: temp_env_overrides(&sandbox_temp),
            })
        }
    }
}

#[cfg(any(windows, test))]
fn dedup_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        let canon = p.canonicalize().unwrap_or(p);
        if !out.iter().any(|e: &PathBuf| e == &canon) {
            out.push(canon);
        }
    }
    out
}

#[cfg(windows)]
fn profile_name_for(request: &ProcessRequest) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    PROFILE_PREFIX.hash(&mut h);
    request.cwd.hash(&mut h);
    if let Some(w) = &request.write_root {
        w.hash(&mut h);
    }
    format!("{PROFILE_PREFIX}.{:x}", h.finish())
}

#[cfg(any(windows, test))]
fn build_cmdline(program: &str, args: &[String]) -> String {
    let mut parts = vec![quote_win(program)];
    for a in args {
        parts.push(quote_win(a));
    }
    parts.join(" ")
}

#[cfg(any(windows, test))]
fn quote_win(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".into();
    }
    if s.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

#[cfg(windows)]
fn cap_bytes(raw: Vec<u8>, cap: usize) -> (String, u64) {
    if raw.len() <= cap {
        return (String::from_utf8_lossy(&raw).into_owned(), 0);
    }
    let head = cap / 2;
    let tail = cap - head;
    let mut out = String::from_utf8_lossy(&raw[..head]).into_owned();
    let dropped = (raw.len() - cap) as u64;
    out.push_str(&format!("\n…[{dropped} bytes dropped]…\n"));
    out.push_str(&String::from_utf8_lossy(&raw[raw.len() - tail..]));
    (out, dropped)
}

/// Whether AppContainer backends are compiled in.
pub fn appcontainer_backend_linked() -> bool {
    cfg!(windows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_and_cmdline_stable() {
        assert_eq!(quote_win("cmd"), "cmd");
        assert_eq!(quote_win("a b"), "\"a b\"");
        let line = build_cmdline("cmd", &["/C".into(), "echo hi".into()]);
        assert!(line.contains("cmd") && line.contains("/C"));
    }

    fn sys_tmp_canon() -> PathBuf {
        std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir())
    }

    fn path_in(list: &[PathBuf], needle: &std::path::Path) -> bool {
        let n = needle
            .canonicalize()
            .unwrap_or_else(|_| needle.to_path_buf());
        list.iter().any(|p| {
            let p = p.canonicalize().unwrap_or_else(|_| p.clone());
            p == n || n.starts_with(&p) || p.starts_with(&n)
        })
    }

    #[test]
    fn plan_ro_workspace_not_writable_but_private_temp_is() {
        let dir = tempfile::tempdir().unwrap();
        let intent = FilesystemIntent::ReadOnly {
            read_roots: vec![dir.path().to_path_buf()],
        };
        let req = ProcessRequest::new("cmd", vec![], dir.path().to_path_buf());
        let plan = plan_for_intent(&intent, &req, &std::env::temp_dir()).unwrap();
        // Workspace itself is not a write root.
        let ws = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        assert!(
            !plan
                .write_roots
                .iter()
                .any(|p| p.canonicalize().unwrap_or_else(|_| p.clone()) == ws),
            "RO must not write-grant workspace; writes={:?}",
            plan.write_roots
        );
        // Private sandbox temp is the only intentional write root (for GetTempPath).
        assert!(
            path_in(&plan.write_roots, &plan.sandbox_temp),
            "sandbox_temp must be write-granted under RO; plan={plan:?}"
        );
        let temp = plan
            .sandbox_temp
            .canonicalize()
            .unwrap_or_else(|_| plan.sandbox_temp.clone());
        let temp_acl_count = acl_roots_for_plan(&plan)
            .iter()
            .filter(|path| path.canonicalize().unwrap_or_else(|_| (*path).clone()) == temp)
            .count();
        assert_eq!(
            temp_acl_count, 1,
            "overlapping read/write roots must be prepared exactly once"
        );
        assert_ne!(
            plan.sandbox_temp
                .canonicalize()
                .unwrap_or(plan.sandbox_temp.clone()),
            sys_tmp_canon(),
            "sandbox_temp must not be whole temp_dir"
        );
        assert!(
            !plan.read_roots.iter().any(|p| p == &sys_tmp_canon()),
            "RO must not grant whole temp_dir; reads={:?}",
            plan.read_roots
        );
    }

    #[test]
    fn plan_ww_sandbox_temp_in_writes_and_temp_env() {
        let dir = tempfile::tempdir().unwrap();
        let intent = FilesystemIntent::WorkspaceWrite {
            write_root: dir.path().to_path_buf(),
            extra_read_roots: vec![],
        };
        let req = ProcessRequest::new("cmd", vec![], dir.path().to_path_buf());
        let plan = plan_for_intent(&intent, &req, &std::env::temp_dir()).unwrap();
        assert!(
            path_in(&plan.write_roots, &plan.sandbox_temp),
            "WW sandbox_temp ∈ write_roots; plan={plan:?}"
        );
        assert!(
            path_in(&plan.write_roots, dir.path()),
            "write_root ∈ write_roots; plan={plan:?}"
        );
        assert_ne!(
            plan.sandbox_temp
                .canonicalize()
                .unwrap_or(plan.sandbox_temp.clone()),
            sys_tmp_canon(),
            "sandbox_temp must not be whole temp_dir"
        );
        assert!(
            !plan.write_roots.iter().any(|p| p == &sys_tmp_canon()),
            "WW must not grant whole temp_dir; writes={:?}",
            plan.write_roots
        );
        // TEMP/TMP/TMPDIR all point at the same granted sandbox_temp.
        let temp_path = plan.sandbox_temp.as_os_str();
        for key in ["TEMP", "TMP", "TMPDIR"] {
            let v = plan
                .env_overrides
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_os_str());
            assert_eq!(
                v,
                Some(temp_path),
                "{key} must equal sandbox_temp; overrides={:?}",
                plan.env_overrides
            );
        }
    }

    #[test]
    fn plan_ro_temp_env_matches_sandbox_temp() {
        let dir = tempfile::tempdir().unwrap();
        let intent = FilesystemIntent::ReadOnly {
            read_roots: vec![dir.path().to_path_buf()],
        };
        let req = ProcessRequest::new("cmd", vec![], dir.path().to_path_buf());
        let plan = plan_for_intent(&intent, &req, &std::env::temp_dir()).unwrap();
        let temp_path = plan.sandbox_temp.as_os_str();
        for key in ["TEMP", "TMP", "TMPDIR"] {
            let v = plan
                .env_overrides
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_os_str());
            assert_eq!(v, Some(temp_path), "{key} override missing/wrong");
        }
    }

    #[test]
    fn plan_uses_a_unique_temp_for_each_command() {
        let dir = tempfile::tempdir().unwrap();
        let intent = FilesystemIntent::ReadOnly {
            read_roots: vec![dir.path().to_path_buf()],
        };
        let req = ProcessRequest::new("cmd", vec![], dir.path().to_path_buf());
        let first = plan_for_intent(&intent, &req, &std::env::temp_dir()).unwrap();
        let second = plan_for_intent(&intent, &req, &std::env::temp_dir()).unwrap();
        assert_ne!(first.sandbox_temp, second.sandbox_temp);
        let _ = std::fs::remove_dir_all(first.sandbox_temp);
        let _ = std::fs::remove_dir_all(second.sandbox_temp);
    }

    #[test]
    fn plan_does_not_authorize_unrelated_sibling_temp() {
        let ws = tempfile::tempdir().unwrap();
        let sib = tempfile::tempdir().unwrap();
        let intent = FilesystemIntent::ReadOnly {
            read_roots: vec![ws.path().to_path_buf()],
        };
        let req = ProcessRequest::new("cmd", vec![], ws.path().to_path_buf());
        let plan = plan_for_intent(&intent, &req, &std::env::temp_dir()).unwrap();
        let sib_c = sib
            .path()
            .canonicalize()
            .unwrap_or_else(|_| sib.path().to_path_buf());
        let all: Vec<_> = plan
            .read_roots
            .iter()
            .chain(plan.write_roots.iter())
            .cloned()
            .collect();
        assert!(
            !all.iter().any(|p| {
                let p = p.canonicalize().unwrap_or_else(|_| p.clone());
                p == sib_c || sib_c.starts_with(&p)
            }),
            "sibling temp must stay outside allowlist; roots={all:?} sibling={sib_c:?}"
        );
    }

    #[test]
    fn backend_linked_matches_cfg() {
        assert_eq!(appcontainer_backend_linked(), cfg!(windows));
    }

    /// WS3-A/B canaries: real AppContainer isolation (Windows CI only).
    ///
    /// Sibling uses a **separate** tempfile root so it is not under any granted
    /// path (workspace, private sandbox temp, or tool-cache). Success asserts
    /// require positive content/file proof — never soft-OR with mere exit code.
    #[cfg(windows)]
    mod canaries {
        use super::*;
        use crate::command::CommandRunner;
        use std::time::Duration;

        fn canary_runner() -> CommandRunner {
            CommandRunner::with_environment(std::sync::Arc::new(leveler_core::EnvSnapshot::new(
                std::env::vars_os(),
                std::env::current_dir().unwrap_or_default(),
                std::env::temp_dir(),
            )))
        }

        struct Pair {
            _ws_keep: tempfile::TempDir,
            _sib_keep: tempfile::TempDir,
            ws: PathBuf,
            sibling: PathBuf,
        }

        fn temp_pair() -> Pair {
            let ws_keep = tempfile::tempdir().expect("ws temp");
            let sib_keep = tempfile::tempdir().expect("sibling temp");
            let ws = ws_keep.path().to_path_buf();
            let sibling = sib_keep.path().to_path_buf();
            std::fs::write(ws.join("secret.txt"), "workspace-ok").unwrap();
            std::fs::write(sibling.join("leak.txt"), "sibling-secret").unwrap();
            // Prove sibling is not under workspace (or vice versa).
            assert!(!sibling.starts_with(&ws) && !ws.starts_with(&sibling));
            Pair {
                _ws_keep: ws_keep,
                _sib_keep: sib_keep,
                ws,
                sibling,
            }
        }

        #[tokio::test]
        async fn readonly_can_read_workspace_but_not_write() {
            let pair = temp_pair();
            let intent = FilesystemIntent::ReadOnly {
                read_roots: vec![pair.ws.clone()],
            };
            let mut req = ProcessRequest::new(
                "cmd",
                vec![
                    "/C".into(),
                    format!("type {}", pair.ws.join("secret.txt").display()),
                ],
                pair.ws.clone(),
            );
            req.filesystem_intent = Some(intent.clone());
            req.timeout = Duration::from_secs(30);
            let out = canary_runner()
                .run(req, CancellationToken::new())
                .await
                .expect("RO read spawn");
            // Positive content proof only — not soft-OR with exit success alone.
            assert!(
                out.stdout.contains("workspace-ok"),
                "RO must read pre-existing child; stdout={} stderr={} exit={:?}",
                out.stdout,
                out.stderr,
                out.exit_code
            );

            let out_path = pair.ws.join("out.txt");
            let mut wreq = ProcessRequest::new(
                "cmd",
                vec!["/C".into(), format!("echo pwned> {}", out_path.display())],
                pair.ws.clone(),
            );
            wreq.filesystem_intent = Some(intent);
            wreq.timeout = Duration::from_secs(30);
            let wout = canary_runner()
                .run(wreq, CancellationToken::new())
                .await
                .expect("RO write spawn");
            let wrote = out_path.exists()
                && std::fs::read_to_string(&out_path)
                    .map(|s| s.contains("pwned"))
                    .unwrap_or(false);
            assert!(
                !wrote,
                "ReadOnly must not allow workspace writes; exit={:?} stderr={}",
                wout.exit_code, wout.stderr
            );
        }

        #[tokio::test]
        async fn readonly_cannot_read_sibling() {
            let pair = temp_pair();
            // Explicitly prove plan_for_intent does not authorize sibling.
            let intent = FilesystemIntent::ReadOnly {
                read_roots: vec![pair.ws.clone()],
            };
            let req_probe = ProcessRequest::new("cmd", vec![], pair.ws.clone());
            let plan = plan_for_intent(&intent, &req_probe, &std::env::temp_dir()).unwrap();
            let sib_c = pair
                .sibling
                .canonicalize()
                .unwrap_or_else(|_| pair.sibling.clone());
            let all: Vec<_> = plan
                .read_roots
                .iter()
                .chain(plan.write_roots.iter())
                .cloned()
                .collect();
            assert!(
                !all.iter().any(|p| p == &sib_c || sib_c.starts_with(p)),
                "canary invalid: sibling is inside allowlist roots={all:?}"
            );

            let mut req = ProcessRequest::new(
                "cmd",
                vec![
                    "/C".into(),
                    format!("type {}", pair.sibling.join("leak.txt").display()),
                ],
                pair.ws.clone(),
            );
            req.filesystem_intent = Some(intent);
            req.timeout = Duration::from_secs(30);
            let out = canary_runner()
                .run(req, CancellationToken::new())
                .await
                .expect("RO sibling spawn");
            assert!(
                !out.stdout.contains("sibling-secret"),
                "sibling content must not leak: stdout={} stderr={}",
                out.stdout,
                out.stderr
            );
        }

        #[tokio::test]
        async fn workspace_write_can_write_workspace_not_sibling() {
            let pair = temp_pair();
            let intent = FilesystemIntent::WorkspaceWrite {
                write_root: pair.ws.clone(),
                extra_read_roots: vec![],
            };
            let req_probe = ProcessRequest::new("cmd", vec![], pair.ws.clone());
            let plan = plan_for_intent(&intent, &req_probe, &std::env::temp_dir()).unwrap();
            let sib_c = pair
                .sibling
                .canonicalize()
                .unwrap_or_else(|_| pair.sibling.clone());
            assert!(
                !plan
                    .write_roots
                    .iter()
                    .any(|p| p == &sib_c || sib_c.starts_with(p)),
                "canary invalid: sibling is inside write roots writes={:?}",
                plan.write_roots
            );

            let target = pair.ws.join("created.txt");
            let mut req = ProcessRequest::new(
                "cmd",
                vec!["/C".into(), format!("echo hello> {}", target.display())],
                pair.ws.clone(),
            );
            req.filesystem_intent = Some(intent.clone());
            req.write_root = Some(pair.ws.clone());
            req.timeout = Duration::from_secs(30);
            let out = canary_runner()
                .run(req, CancellationToken::new())
                .await
                .expect("WW write spawn");
            // Positive file content proof only.
            let content = std::fs::read_to_string(&target).unwrap_or_default();
            assert!(
                content.contains("hello"),
                "workspace write must create file with content; exit={:?} stderr={} content={content:?}",
                out.exit_code,
                out.stderr
            );

            // Per-command temp: prove the child can write/read through %TEMP%,
            // that the path is a private child of the host temp root, and that
            // the host temp root itself was not used as the write target.
            let probe_name = "leveler_temp_probe.txt";
            let host_probe = std::env::temp_dir().join(probe_name);
            let _ = std::fs::remove_file(&host_probe);
            let mut treq = ProcessRequest::new(
                "cmd",
                vec![
                    "/C".into(),
                    format!(
                        "echo %TEMP%&& echo temp-ok>%TEMP%\\{probe_name}&& type %TEMP%\\{probe_name}"
                    ),
                ],
                pair.ws.clone(),
            );
            treq.filesystem_intent = Some(intent.clone());
            treq.write_root = Some(pair.ws.clone());
            treq.timeout = Duration::from_secs(30);
            let tout = canary_runner()
                .run(treq, CancellationToken::new())
                .await
                .expect("WW %TEMP% spawn");
            assert!(
                tout.stdout.contains("temp-ok"),
                "%TEMP% write/read must succeed; stdout={} exit={:?} stderr={}",
                tout.stdout,
                tout.exit_code,
                tout.stderr
            );
            let child_temp = tout
                .stdout
                .lines()
                .map(str::trim)
                .find(|line| line.to_ascii_lowercase().contains("leveler-ac-"))
                .expect("child must report its private %TEMP% path");
            let child_temp = child_temp.to_ascii_lowercase();
            let host_temp = std::env::temp_dir()
                .display()
                .to_string()
                .trim_end_matches(['\\', '/'])
                .to_ascii_lowercase();
            assert!(
                child_temp.starts_with(&host_temp) && child_temp != host_temp,
                "child TEMP must be private under host temp: child={child_temp:?} host={host_temp:?}"
            );
            // Must not create the probe at the host system temp root.
            let host_pwned = host_probe.exists()
                && std::fs::read_to_string(&host_probe)
                    .map(|s| s.contains("temp-ok"))
                    .unwrap_or(false);
            assert!(
                !host_pwned,
                "must not write probe into host temp_dir(); found {host_probe:?}"
            );

            let evil = pair.sibling.join("pwn.txt");
            let mut sreq = ProcessRequest::new(
                "cmd",
                vec!["/C".into(), format!("echo bad> {}", evil.display())],
                pair.ws.clone(),
            );
            sreq.filesystem_intent = Some(intent);
            sreq.write_root = Some(pair.ws.clone());
            sreq.timeout = Duration::from_secs(30);
            let _ = canary_runner()
                .run(sreq, CancellationToken::new())
                .await
                .expect("WW sibling spawn");
            let sibling_pwned = evil.exists()
                && std::fs::read_to_string(&evil)
                    .map(|s| s.contains("bad"))
                    .unwrap_or(false);
            assert!(
                !sibling_pwned,
                "sibling write must fail under write-restricted intent"
            );
            let _ = std::fs::remove_dir_all(plan.sandbox_temp);
        }

        #[tokio::test]
        async fn deny_network_true_is_accepted_for_appcontainer() {
            let pair = temp_pair();
            let intent = FilesystemIntent::ReadOnly {
                read_roots: vec![pair.ws.clone()],
            };
            let mut req = ProcessRequest::new(
                "cmd",
                vec!["/C".into(), "echo net-ok".into()],
                pair.ws.clone(),
            );
            req.filesystem_intent = Some(intent);
            req.deny_network = true;
            req.timeout = Duration::from_secs(30);
            let out = canary_runner()
                .run(req, CancellationToken::new())
                .await
                .expect("deny_network AppContainer spawn");
            assert!(
                out.stdout.contains("net-ok"),
                "deny_network must still run; stdout={} stderr={}",
                out.stdout,
                out.stderr
            );
        }
    }
}
