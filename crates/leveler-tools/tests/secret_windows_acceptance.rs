//! Windows-only evidence run for the Secret Broker file boundary.
//!
//! This test is driven by `scripts/secret_broker_windows_acceptance.ps1`. It
//! creates a real workspace under `LEVELER_SECRET_ACCEPTANCE_ROOT`, writes
//! through the real broker tool, and records the raw `icacls` output and the
//! check matrix in `LEVELER_SECRET_ACCEPTANCE_EVIDENCE`. It deliberately does
//! not replace the unit suites: it is the host-level artifact a reviewer can
//! inspect after the fact.
//!
//! Every value is synthetic. The test never prints the canary.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leveler_execution::PermissionProfile;
use leveler_execution::secret_vault::{SecretRef, SecretVault};
use leveler_tools::tools::{SecretApplyTool, SecretInspectTool};
use leveler_tools::{Tool, ToolContext};
use serde_json::json;

const DEFAULT_CANARY: &str = "CANARY-SECRET-DO-NOT-EXPOSE-001";

fn system_exe(name: &str) -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join(name)
}

fn run(program: &Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new(program)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("{}: {error}", program.display()));
    format!(
        "exit={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn icacls(path: &Path) -> String {
    run(&system_exe("icacls.exe"), &[&path.display().to_string()])
}

fn workspace_at(root: &Path) -> leveler_execution::Workspace {
    leveler_execution::Workspace::new(root).expect("workspace")
}

struct Harness {
    vault: Arc<SecretVault>,
    context: ToolContext,
}

impl Harness {
    fn new(root: &Path) -> Self {
        let vault = Arc::new(SecretVault::new());
        let context = ToolContext::new(workspace_at(root), PermissionProfile::Assisted)
            .with_session_scope("windows-acceptance")
            .with_secret_vault(vault.clone());
        Self { vault, context }
    }

    fn capture(&self, value: &str) -> SecretRef {
        self.vault
            .capture("windows-acceptance", value.to_string(), None)
            .expect("capture")
    }

    async fn apply(&self, reference: &SecretRef, field: &str, target: &str) -> (bool, String) {
        let output = SecretApplyTool
            .execute(
                json!({
                    "secret_ref": reference.as_str(),
                    "field": field,
                    "target": target,
                }),
                self.context.clone(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("tool result");
        (output.is_error, output.content)
    }

    async fn inspect(&self, field: &str, target: &str) -> (bool, String) {
        let output = SecretInspectTool
            .execute(
                json!({ "field": field, "target": target }),
                self.context.clone(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("tool result");
        (output.is_error, output.content)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_secret_file_boundary_evidence() {
    let Ok(root) = std::env::var("LEVELER_SECRET_ACCEPTANCE_ROOT") else {
        eprintln!("skipped: LEVELER_SECRET_ACCEPTANCE_ROOT is not set");
        return;
    };
    let root = PathBuf::from(root);
    std::fs::create_dir_all(&root).expect("acceptance root");
    let evidence = std::env::var_os("LEVELER_SECRET_ACCEPTANCE_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("windows-acceptance.json"));
    let canary = std::env::var("LEVELER_SECRET_ACCEPTANCE_CANARY")
        .unwrap_or_else(|_| DEFAULT_CANARY.to_string());
    let harness = Harness::new(&root);
    let reference = harness.capture(&canary);
    // The person's own statements for this capture. `.env.production` is not
    // among the positive targets; the Chinese path is explicitly named.
    harness.vault.authorize_use(
        "windows-acceptance",
        &reference,
        &[".env".to_string(), "配置 目录/.env".to_string()],
        &["DEMO_API_KEY".to_string()],
    );
    let mut checks = Vec::new();
    let mut acls = serde_json::Map::new();

    // 1. A new file in a directory that grants Everyone read must not inherit
    //    the broad DACL.
    let broad_dir = root.join("broad");
    std::fs::create_dir_all(&broad_dir).unwrap();
    let grant = run(
        &system_exe("icacls.exe"),
        &[
            &broad_dir.display().to_string(),
            "/grant",
            "*S-1-1-0:(OI)(CI)R",
        ],
    );
    let broad_workspace = Harness::new(&broad_dir);
    let broad_reference = broad_workspace.capture(&canary);
    let (error, content) = broad_workspace
        .apply(&broad_reference, "DEMO_API_KEY", ".env")
        .await;
    let broad_env = broad_dir.join(".env");
    let broad_private = leveler_win_confine::verify_private_state_file(&broad_env).is_ok();
    checks.push(json!({
        "name": "new file does not inherit a broad parent DACL",
        "ok": !error && broad_private,
        "detail": if error { content } else { String::new() },
    }));
    acls.insert("broad-parent".into(), json!(grant));
    acls.insert("broad-new-env".into(), json!(icacls(&broad_env)));

    // 2. Updating an existing broad .env tightens it.
    let update_dir = root.join("update");
    std::fs::create_dir_all(&update_dir).unwrap();
    let update_env = update_dir.join(".env");
    std::fs::write(&update_env, "DEMO_API_KEY=old-value\n").unwrap();
    let widen = run(
        &system_exe("icacls.exe"),
        &[&update_env.display().to_string(), "/grant", "*S-1-1-0:R"],
    );
    let update_workspace = Harness::new(&update_dir);
    let update_reference = update_workspace.capture(&canary);
    let (error, content) = update_workspace
        .apply(&update_reference, "DEMO_API_KEY", ".env")
        .await;
    let update_text = std::fs::read_to_string(&update_env).unwrap_or_default();
    let update_private = leveler_win_confine::verify_private_state_file(&update_env).is_ok();
    checks.push(json!({
        "name": "an existing broad .env is tightened while the value changes",
        "ok": !error && update_private && update_text.contains(&canary),
        "detail": if error { content } else { String::new() },
    }));
    acls.insert("existing-before".into(), json!(widen));
    acls.insert("existing-after".into(), json!(icacls(&update_env)));

    // 3. A target the person did not authorize is refused and untouched.
    let denied_target = root.join(".env.production");
    let (error, content) = harness
        .apply(&reference, "DEMO_API_KEY", ".env.production")
        .await;
    checks.push(json!({
        "name": "an over-scope target is refused without a file",
        "ok": error && !denied_target.exists(),
        "detail": content,
    }));

    // 4. An explicit prohibition is enforced in Full as well.
    harness.vault.authorize_declaration(
        "windows-acceptance",
        &reference,
        &leveler_core::DotenvUseDeclaration {
            targets: vec![".env".to_string()],
            fields: vec!["DEMO_API_KEY".to_string()],
            denied_targets: vec![".env.production".to_string()],
            ..Default::default()
        },
    );
    let (error, content) = harness
        .apply(&reference, "DEMO_API_KEY", ".env.production")
        .await;
    checks.push(json!({
        "name": "an explicit prohibition blocks the target",
        "ok": error && !denied_target.exists(),
        "detail": content,
    }));

    // 5. A Chinese path with spaces works and stays private.
    let chinese_dir = root.join("配置 目录");
    std::fs::create_dir_all(&chinese_dir).unwrap();
    let (error, content) = harness
        .apply(&reference, "DEMO_API_KEY", "配置 目录/.env")
        .await;
    let chinese_env = chinese_dir.join(".env");
    let chinese_private = leveler_win_confine::verify_private_state_file(&chinese_env).is_ok();
    checks.push(json!({
        "name": "a Chinese path with spaces is private",
        "ok": !error && chinese_private,
        "detail": if error { content } else { String::new() },
    }));
    acls.insert("chinese-env".into(), json!(icacls(&chinese_env)));

    // 6. A junction cannot redirect the write outside the workspace. This host
    //    refuses reparse-point creation from an unsigned test process, so the
    //    acceptance script creates the junction and passes its path. The test
    //    still exercises the broker against the real reparse point.
    let junction_root = root.join("junction");
    std::fs::create_dir_all(&junction_root).unwrap();
    let mut outside_guard: Option<tempfile::TempDir> = None;
    let junction = match std::env::var_os("LEVELER_SECRET_ACCEPTANCE_JUNCTION")
        .map(PathBuf::from)
        .filter(|path| path.exists())
    {
        Some(path) => Some(path),
        None => {
            let junction = junction_root.join("escape");
            let _ = std::fs::remove_dir(&junction);
            let outside = tempfile::tempdir().unwrap();
            let native = |path: &Path| path.display().to_string().replace('/', "\\");
            let script_dir = tempfile::tempdir().unwrap();
            let script = script_dir.path().join("mklink.cmd");
            std::fs::write(&script, "@echo off\r\nmklink /J \"%~1\" \"%~2\"\r\n").unwrap();
            let link = std::process::Command::new(system_exe("cmd.exe"))
                .arg("/c")
                .arg(&script)
                .arg(native(&junction))
                .arg(native(outside.path()))
                .output()
                .expect("junction");
            if link.status.success() && junction.exists() {
                outside_guard = Some(outside);
                Some(junction)
            } else {
                None
            }
        }
    };
    if let Some(junction) = junction {
        let outside = junction
            .canonicalize()
            .expect("the junction resolves to its target");
        let outside_env = outside.join(".env");
        std::fs::write(&outside_env, "UNCHANGED=1\n").unwrap();
        let target = format!(
            "{}/.env",
            junction
                .strip_prefix(&root)
                .expect("junction is inside the workspace")
                .display()
                .to_string()
                .replace('\\', "/")
        );
        // The person explicitly names the junction path. The scope therefore
        // authorizes it, and the refusal must come from the workspace's
        // reparse-point authority, not from a missing scope entry.
        harness.vault.authorize_use(
            "windows-acceptance",
            &reference,
            &[target.clone()],
            &["DEMO_API_KEY".to_string()],
        );
        let (error, content) = harness.apply(&reference, "DEMO_API_KEY", &target).await;
        let outside_text = std::fs::read_to_string(&outside_env).unwrap_or_default();
        checks.push(json!({
            "name": "a junction cannot redirect the credential write",
            "ok": error && outside_text == "UNCHANGED=1\n",
            "detail": content,
        }));
        acls.insert("junction-outside".into(), json!(icacls(&outside_env)));
        checks.push(json!({
            "name": "a junction is refused as a private state directory",
            "ok": leveler_win_confine::secure_private_state_dir(&junction).is_err(),
            "detail": junction.display().to_string(),
        }));
    } else {
        checks.push(json!({
            "name": "a junction cannot redirect the credential write",
            "ok": false,
            "detail": "BLOCKED: this host refused reparse-point creation from the test \
                       process; run scripts/secret_broker_windows_acceptance.ps1, which \
                       creates the junction from the script host and passes \
                       LEVELER_SECRET_ACCEPTANCE_JUNCTION",
        }));
    }
    drop(outside_guard);

    // 7. Inspector confirms presence without the value.
    let (error, content) = harness.inspect("DEMO_API_KEY", "配置 目录/.env").await;
    checks.push(json!({
        "name": "secret_inspect confirms presence without the value",
        "ok": !error && !content.contains(&canary),
        "detail": content,
    }));

    // 8. No leftover staged file contains the canary anywhere under the root.
    let mut leaked = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory)
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(".leveler-tmp"))
            {
                leaked.push(path.display().to_string());
            }
        }
    }
    checks.push(json!({
        "name": "no staged credential file remains",
        "ok": leaked.is_empty(),
        "detail": leaked.join(", "),
    }));

    let all_ok = checks
        .iter()
        .all(|check| check["ok"].as_bool().unwrap_or(false));
    let document = json!({
        "test": "windows_secret_file_boundary_evidence",
        "identity": run(&system_exe("whoami.exe"), &["/user"]),
        "groups": run(&system_exe("whoami.exe"), &["/groups", "/fo", "csv"]),
        "privileges": run(&system_exe("whoami.exe"), &["/priv"]),
        "checks": checks,
        "acls": acls,
        "all_ok": all_ok,
    });
    if let Some(parent) = evidence.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&evidence, serde_json::to_string_pretty(&document).unwrap()).unwrap();
    assert!(all_ok, "one or more Windows boundary checks failed");
}
