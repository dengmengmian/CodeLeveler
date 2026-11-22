//! Real Windows self-approval canary for the trusted-definition store.
//!
//! The attacker is the production `CommandRunner` + `leveler-confine.exe`
//! path, running the real CLI binary at Low integrity with the same arguments
//! an agent would use. The command may legitimately succeed against the
//! per-command sandbox home (that home is Low-writable by design); what must
//! never change is the store the runtime reads. This test records both facts
//! instead of assuming that a successful CLI invocation approved anything.
//!
//! Run explicitly after building the launcher:
//!
//! ```text
//! cargo build -p leveler-win-confine --bins
//! LEVELER_CANARY_EVIDENCE=C:\qlab\...\evidence cargo test -p leveler-cli \
//!   --test windows_trust_self_approval -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leveler_core::EnvSnapshot;
use leveler_execution::command::{CommandRunner, ProcessRequest};
use leveler_execution::risk::WriteScope;
use tokio_util::sync::CancellationToken;

const RULE: &str = "EVIL-SELF-APPROVAL-MARKER\n";

fn evidence_dir(base: &Path) -> PathBuf {
    std::env::var_os("LEVELER_CANARY_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| base.join("evidence"))
}

fn record(evidence: &Path, label: &str, body: &str) {
    std::fs::create_dir_all(evidence).unwrap();
    std::fs::write(evidence.join(format!("{label}.txt")), body).unwrap();
}

fn view_state(home: &Path, workspace: &Path) -> String {
    use leveler_definition_trust::{DefinitionIdentity, DefinitionKind, DefinitionTrustView};
    let identity =
        DefinitionIdentity::project(DefinitionKind::RuleFile, ".leveler/rules/rule.md").unwrap();
    let digest = DefinitionIdentity::single_digest(RULE.as_bytes());
    DefinitionTrustView::load(home, Some(workspace))
        .check(&identity, &digest)
        .audit_state()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows self-approval canary; needs leveler-confine.exe and the CLI binary"]
async fn a_confined_cli_approval_does_not_reach_the_runtime_store() {
    if leveler_execution::windows_confine::launcher_path().is_none() {
        panic!("leveler-confine.exe is not built beside the test binary");
    }
    let base = std::env::temp_dir().join(format!("leveler-selfapprove-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let workspace = base.join("workspace");
    let home = base.join("home");
    std::fs::create_dir_all(workspace.join(".leveler/rules")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(workspace.join(".leveler/rules/rule.md"), RULE).unwrap();
    let evidence = evidence_dir(&base);
    // The candidate under test can be the debug test binary or the release
    // artifact; both are the same source. The sealed store boundary does not
    // depend on the profile.
    let cli =
        std::env::var_os("LEVELER_CLI_BIN").unwrap_or_else(|| env!("CARGO_BIN_EXE_leveler").into());
    let cli_display = cli.to_string_lossy().into_owned();

    let mut vars: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os().collect();
    vars.retain(|(key, _)| key != "LEVELER_HOME");
    vars.push((
        std::ffi::OsString::from("LEVELER_HOME"),
        home.as_os_str().to_os_string(),
    ));
    let environment = Arc::new(EnvSnapshot::new(
        vars,
        workspace.clone(),
        std::env::temp_dir(),
    ));
    let runner = CommandRunner::with_environment(environment);

    let mut request = ProcessRequest::new(
        cli_display.clone(),
        vec![
            "trust".to_string(),
            "allow".to_string(),
            "--yes".to_string(),
        ],
        workspace.clone(),
    );
    request.write_scope = WriteScope::Workspace {
        root: workspace.clone(),
    };
    let output = runner
        .run(request, CancellationToken::new())
        .await
        .expect("the confined CLI must start");

    let store = home.join("definition-trust.yaml");
    let config_store = home.join("trusted.yaml");
    let runtime_view = view_state(&home, &workspace);
    let store_written = store.exists() || config_store.exists();
    let body = format!(
        "workspace: {}\nhome: {}\nprogram: {}\nargv: trust allow --yes\n\
         exit: {:?}\nstore_written: {store_written}\nruntime_view: {runtime_view}\n\
         stdout:\n{}\nstderr:\n{}\n",
        workspace.display(),
        home.display(),
        cli_display,
        output.exit_code,
        output.stdout,
        output.stderr,
    );
    record(&evidence, "confined-self-approval", &body);
    eprintln!("{body}");

    assert!(
        !store_written,
        "the confined CLI must not write the store the runtime reads"
    );
    assert_eq!(
        runtime_view, "unapproved",
        "the runtime's store must still deny the definition"
    );

    // Second route: the child overrides the runner's private LEVELER_HOME and
    // points the CLI at the store the runtime actually reads. The Low token
    // cannot write the Medium home, so this must fail at the OS boundary.
    let direct = format!(
        "@echo off\r\nset LEVELER_HOME={home}\r\n\"{cli}\" trust allow --yes > direct-store.txt 2>&1\r\necho EXIT=%ERRORLEVEL% >> direct-store.txt\r\n",
        home = home.display(),
        cli = cli_display,
    );
    std::fs::write(workspace.join("direct-store.cmd"), direct).unwrap();
    let direct_script = workspace.join("direct-store.cmd").display().to_string();
    let mut direct_request = ProcessRequest::new(
        "cmd",
        vec!["/C".to_string(), direct_script],
        workspace.clone(),
    );
    direct_request.write_scope = WriteScope::Workspace {
        root: workspace.clone(),
    };
    let direct_output = runner
        .run(direct_request, CancellationToken::new())
        .await
        .expect("the direct-store command must start");
    let direct_text =
        std::fs::read_to_string(workspace.join("direct-store.txt")).unwrap_or_default();
    let direct_evidence = format!(
        "exit: {:?}\nstore_written: {}\nruntime_view: {}\noutput:\n{}\n",
        direct_output.exit_code,
        store.exists() || config_store.exists(),
        view_state(&home, &workspace),
        direct_text,
    );
    record(&evidence, "direct-store-attack", &direct_evidence);
    eprintln!("{direct_evidence}");
    assert!(
        direct_text.contains("EXIT=1"),
        "the direct store write must fail at the OS boundary: {direct_text}"
    );
    assert!(
        !store.exists() && !config_store.exists(),
        "the real store must not be written"
    );
    assert_eq!(view_state(&home, &workspace), "unapproved");

    // Control: the same CLI, run unconfined as the user's own command, is the
    // human approval surface and does change the store this test reads.
    let host = std::process::Command::new(&cli)
        .env("LEVELER_HOME", &home)
        .arg("--repo")
        .arg(&workspace)
        .args(["trust", "allow", "--yes"])
        .output()
        .expect("the host CLI must run");
    let host_out = format!(
        "exit: {:?}\nstdout:\n{}\nstderr:\n{}\n",
        host.status,
        String::from_utf8_lossy(&host.stdout),
        String::from_utf8_lossy(&host.stderr)
    );
    record(&evidence, "host-approval", &host_out);
    assert!(host.status.success(), "{host_out}");
    assert_eq!(view_state(&home, &workspace), "trusted", "{host_out}");
    assert!(store.exists(), "the host approval must write the store");

    let _ = std::fs::remove_dir_all(&base);
}
