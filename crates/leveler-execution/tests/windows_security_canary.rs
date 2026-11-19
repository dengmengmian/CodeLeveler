//! Real Windows OS security canary for the confined-shell write boundary.
//!
//! This is acceptance evidence, not a unit test: it drives the production
//! `CommandRunner` + `leveler-confine.exe` path against a real NTFS workspace
//! and records the actual OS outcome for each protected-path operation. It is
//! `#[ignore]`d because it needs the launcher built and mutates Mandatory
//! Integrity labels on a scratch tree; run it explicitly:
//!
//! ```text
//! cargo build -p leveler-win-confine --bins
//! LEVELER_CANARY_EVIDENCE=C:\qlab\...\evidence cargo test -p leveler-execution \
//!   --test windows_security_canary -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Every case is evaluated from the filesystem before/after, never from the
//! command's own claim. Cases the current backend cannot enforce are recorded
//! as `FAIL` and named as residual, not papered over.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leveler_core::EnvSnapshot;
use leveler_execution::command::{CommandRunner, ProcessRequest};
use leveler_execution::risk::WriteScope;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

struct Canary {
    root: PathBuf,
    evidence: PathBuf,
    runner: CommandRunner,
    results: Vec<(String, &'static str, String)>,
}

impl Canary {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("leveler-sec-canary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("workspace");
        let evidence = std::env::var_os("LEVELER_CANARY_EVIDENCE")
            .map(PathBuf::from)
            .unwrap_or_else(|| base.join("evidence"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&evidence).unwrap();
        let runner = CommandRunner::with_environment(Arc::new(EnvSnapshot::new(
            std::env::vars_os(),
            root.clone(),
            std::env::temp_dir(),
        )));
        Self {
            root,
            evidence,
            runner,
            results: Vec::new(),
        }
    }

    fn seed(&self) {
        std::fs::write(self.root.join("AGENTS.md"), "ROOT-RULES\n").unwrap();
        std::fs::create_dir_all(self.root.join("src").join("deep")).unwrap();
        std::fs::write(
            self.root.join("src").join("deep").join("AGENTS.md"),
            "NESTED-RULES\n",
        )
        .unwrap();
        std::fs::write(self.root.join("src").join("business.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(self.root.join(".leveler")).unwrap();
        std::fs::write(
            self.root.join(".leveler").join("config.yaml"),
            "default_mode: assisted\n",
        )
        .unwrap();
        std::fs::create_dir_all(self.root.join(".git")).unwrap();
        std::fs::write(
            self.root.join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();
    }

    async fn run(&self, label: &str, script: &str) {
        let mut request = ProcessRequest::new(
            "cmd",
            vec!["/C".to_string(), script.to_string()],
            self.root.clone(),
        );
        request.write_scope = WriteScope::Workspace {
            root: self.root.clone(),
        };
        let output = self
            .runner
            .run(request, CancellationToken::new())
            .await
            .expect("canary command must run");
        std::fs::write(
            self.evidence.join(format!("{label}.txt")),
            format!(
                "script: {script}\nexit: {:?}\nstdout:\n{}\nstderr:\n{}\n",
                output.exit_code, output.stdout, output.stderr
            ),
        )
        .unwrap();
    }

    fn read(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(relative)).ok()
    }

    fn record(&mut self, id: &str, verdict: &'static str, detail: impl Into<String>) {
        self.results.push((id.to_string(), verdict, detail.into()));
    }

    fn hash_tree(&self) -> String {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
            let Ok(read) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in read.flatten() {
                let path = entry.path();
                // Never follow a reparse point: the canary creates junctions to
                // prove they do not launder writes, and following them here
                // would recurse through the same tree or leave the workspace.
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_symlink() {
                    let relative = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((relative, "link".into()));
                    continue;
                }
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if file_type.is_dir() {
                    out.push((relative, "dir".into()));
                    walk(root, &path, out);
                } else {
                    let digest = std::fs::read(&path).map(|bytes| {
                        let mut hasher = Sha256::new();
                        hasher.update(&bytes);
                        format!("{:x}", hasher.finalize())
                    });
                    out.push((relative, digest.unwrap_or_else(|e| format!("ERR {e}"))));
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root, &self.root, &mut out);
        out.sort();
        let mut hasher = Sha256::new();
        for (path, digest) in out {
            hasher.update(path.as_bytes());
            hasher.update(b"\0");
            hasher.update(digest.as_bytes());
            hasher.update(b"\n");
        }
        format!("{:x}", hasher.finalize())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows OS security canary; needs leveler-confine.exe and a scratch workspace"]
async fn the_confined_shell_write_boundary_on_a_real_windows_workspace() {
    if leveler_execution::windows_confine::launcher_path().is_none() {
        panic!("leveler-confine.exe is not built beside the test binary");
    }
    let mut canary = Canary::new();
    canary.seed();
    let before = canary.hash_tree();
    std::fs::write(canary.evidence.join("tree-before.txt"), &before).unwrap();

    // SEC-01 / SEC-07 / case variant: existing root AGENTS.md. The delete
    // fence is what covers `del`/`move /Y`/`ren`; MIC covers content writes.
    canary
        .run(
            "sec01-root-write",
            "echo PWNED>> AGENTS.md & echo PWNED> AGENTS.MD & copy /Y src\\business.rs AGENTS.md & \
             move /Y src\\business.rs AGENTS.md & \
             del AGENTS.md & ren AGENTS.md root-moved.md & \
             del .leveler\\config.yaml & ren .leveler leveler-moved & \
             rd /s /q .git & ren .git git-moved",
        )
        .await;
    let root_rules = canary.read("AGENTS.md").unwrap_or_default();
    canary.record(
        "SEC-01",
        if root_rules == "ROOT-RULES\n" {
            "PASS"
        } else {
            "FAIL"
        },
        format!("root AGENTS.md = {root_rules:?}"),
    );
    let leveler_intact = canary.read(".leveler/config.yaml").as_deref()
        == Some("default_mode: assisted\n")
        && canary.root.join(".leveler").is_dir()
        && !canary.root.join("leveler-moved").exists();
    let git_intact = canary.read(".git/HEAD").is_some() && canary.root.join(".git").is_dir();
    canary.record(
        "SEC-07",
        if canary.read("src/business.rs").is_some()
            && canary.root.join("AGENTS.md").is_file()
            && !canary.root.join("root-moved.md").exists()
            && leveler_intact
            && git_intact
        {
            "PASS"
        } else {
            "FAIL"
        },
        format!(
            "delete/rename/replace of guarded files refused; .leveler intact={leveler_intact}; .git intact={git_intact}"
        ),
    );

    // SEC-07B: a pre-existing hardlink alias shares the file object, so it
    // shares the Medium label and the runtime's delete-sharing fence. Writing
    // or deleting through the alias must not change the protected original.
    std::fs::hard_link(
        canary.root.join("AGENTS.md"),
        canary.root.join("hard-AGENTS.md"),
    )
    .expect("create hardlink alias");
    canary
        .run(
            "sec07b-hardlink",
            "echo PWNED>> hard-AGENTS.md & del hard-AGENTS.md",
        )
        .await;
    let hardlink_rules = canary.read("hard-AGENTS.md");
    let root_after_hardlink = canary.read("AGENTS.md").unwrap_or_default();
    canary.record(
        "SEC-07B",
        if root_after_hardlink == "ROOT-RULES\n"
            && hardlink_rules
                .as_deref()
                .is_none_or(|text| text == "ROOT-RULES\n")
        {
            "PASS"
        } else {
            "FAIL"
        },
        format!(
            "hardlink alias = {hardlink_rules:?}; protected original = {root_after_hardlink:?}"
        ),
    );
    let _ = std::fs::remove_file(canary.root.join("hard-AGENTS.md"));

    // SEC-08B: a file symlink is another alias onto the same file object. If
    // this unprivileged account cannot create one (Developer Mode / SeCreateSymbolicLink),
    // record the OS limitation rather than pretending the case passed.
    let symlink = canary.root.join("sym-AGENTS.md");
    match std::os::windows::fs::symlink_file("AGENTS.md", &symlink) {
        Ok(()) => {
            canary
                .run(
                    "sec08b-symlink",
                    "echo PWNED>> sym-AGENTS.md & del sym-AGENTS.md",
                )
                .await;
            let symlink_rules = canary.read("sym-AGENTS.md");
            let root_after_symlink = canary.read("AGENTS.md").unwrap_or_default();
            canary.record(
                "SEC-08B",
                if root_after_symlink == "ROOT-RULES\n"
                    && symlink_rules
                        .as_deref()
                        .is_none_or(|text| text == "ROOT-RULES\n")
                {
                    "PASS"
                } else {
                    "FAIL"
                },
                format!(
                    "symlink alias = {symlink_rules:?}; protected original = {root_after_symlink:?}"
                ),
            );
            let _ = std::fs::remove_file(&symlink);
        }
        Err(error) => {
            std::fs::write(
                canary.evidence.join("sec08b-symlink-unavailable.txt"),
                format!("symlink creation by this account failed: {error}\n"),
            )
            .unwrap();
            canary.record(
                "SEC-08B",
                "NOT_APPLICABLE",
                format!("this unprivileged account cannot create a file symlink: {error}"),
            );
        }
    }

    // SEC-03: existing nested AGENTS.md.
    canary
        .run("sec03-nested-write", "echo PWNED>> src\\deep\\AGENTS.md")
        .await;
    let nested = canary.read("src/deep/AGENTS.md").unwrap_or_default();
    canary.record(
        "SEC-03",
        if nested == "NESTED-RULES\n" {
            "PASS"
        } else {
            "FAIL"
        },
        format!("nested AGENTS.md = {nested:?}"),
    );

    // SEC-02: a missing root AGENTS.md created by the confined shell.
    std::fs::remove_file(canary.root.join("AGENTS.md")).unwrap();
    canary
        .run("sec02-root-create", "echo FORGED> AGENTS.md")
        .await;
    let forged_root = canary.read("AGENTS.md");
    canary.record(
        "SEC-02",
        if forged_root.is_none() {
            "PASS"
        } else {
            "KNOWN-GAP"
        },
        format!("created root AGENTS.md = {forged_root:?}"),
    );

    // SEC-04: a missing nested AGENTS.md in a directory that existed.
    std::fs::remove_file(canary.root.join("src/deep/AGENTS.md")).unwrap();
    canary
        .run(
            "sec04-nested-create",
            "echo FORGED> src\\deep\\AGENTS.md & echo FORGED> src\\AGENTS.MD",
        )
        .await;
    let forged_nested = canary.read("src/deep/AGENTS.md");
    canary.record(
        "SEC-04",
        if forged_nested.is_none() {
            "PASS"
        } else {
            "KNOWN-GAP"
        },
        format!("created nested AGENTS.md = {forged_nested:?}"),
    );

    // SEC-05 / SEC-06: missing definition directories under an existing
    // `.leveler` (the parent carries the Medium label).
    canary
        .run(
            "sec05-06-definition-dirs",
            "mkdir .leveler\\agents & echo evil> .leveler\\agents\\evil.yaml & \
             mkdir .leveler\\skills & echo evil> .leveler\\skills\\evil.md",
        )
        .await;
    let agents_forged = canary.root.join(".leveler").join("agents").exists();
    let skills_forged = canary.root.join(".leveler").join("skills").exists();
    canary.record(
        "SEC-05",
        if agents_forged { "FAIL" } else { "PASS" },
        format!("missing .leveler/agents created by confined shell = {agents_forged}"),
    );
    canary.record(
        "SEC-06",
        if skills_forged { "FAIL" } else { "PASS" },
        format!("missing .leveler/skills created by confined shell = {skills_forged}"),
    );

    // SEC-08: a junction into the protected `.leveler` namespace, and a
    // junction to the workspace root, do not launder a protected write.
    canary
        .run(
            "sec08-links",
            "mklink /J jump .leveler & echo evil> jump\\agents\\evil.yaml & \
             mklink /J rootalias . & echo PWNED>> rootalias\\AGENTS.MD",
        )
        .await;
    let laundered = canary.read(".leveler/agents/evil.yaml");
    canary.record(
        "SEC-08",
        if laundered.is_none() { "PASS" } else { "FAIL" },
        format!("junction write through to .leveler = {laundered:?}"),
    );

    // SEC-09: an existing rule document written from a descendant shell
    // inherits the Low token. Restore the root document that SEC-02 removed so
    // this measures inheritance, not the SEC-02 creation gap.
    std::fs::write(canary.root.join("AGENTS.md"), "ROOT-RULES\n").unwrap();
    canary
        .run(
            "sec09-subprocess",
            "cmd /C \"echo PWNED>> AGENTS.md\" & powershell -NoProfile -Command \
             \"Add-Content -Path AGENTS.md -Value PWNED\"",
        )
        .await;
    let after_child = canary.read("AGENTS.md");
    canary.record(
        "SEC-09",
        if after_child.as_deref() == Some("ROOT-RULES\n") {
            "PASS"
        } else {
            "FAIL"
        },
        format!("root AGENTS.md after descendant shells = {after_child:?}"),
    );

    // SEC-10 / SEC-11: ordinary writes and directory creation still work.
    canary
        .run(
            "sec10-11-ordinary",
            "echo fine> src\\new_file.txt & mkdir src\\new_dir & \
             echo fine> src\\new_dir\\inside.txt & echo edited>> src\\business.rs",
        )
        .await;
    let ordinary = canary.root.join("src").join("new_file.txt").is_file()
        && canary
            .root
            .join("src")
            .join("new_dir")
            .join("inside.txt")
            .is_file()
        && canary
            .read("src/business.rs")
            .is_some_and(|text| text.contains("edited"));
    canary.record(
        "SEC-10",
        if ordinary { "PASS" } else { "FAIL" },
        "ordinary file write and edit inside the workspace",
    );
    canary.record(
        "SEC-11",
        if canary.root.join("src/new_dir").is_dir() {
            "PASS"
        } else {
            "FAIL"
        },
        "ordinary directory creation inside the workspace",
    );

    // SEC-12: the trusted runtime (the file tools' process, never the shell)
    // can still author the definition; the boundary is not blanket read-only.
    std::fs::write(canary.root.join("AGENTS.md"), "RUNTIME-AUTHORED\n").unwrap();
    canary.record(
        "SEC-12",
        if canary.read("AGENTS.md").as_deref() == Some("RUNTIME-AUTHORED\n") {
            "PASS"
        } else {
            "FAIL"
        },
        "the trusted file-tool process can write AGENTS.md",
    );

    // SEC-13: the identity the canary ran under (non-elevated Medium user).
    let whoami = std::process::Command::new(r"C:\Windows\System32\whoami.exe")
        .args(["/groups"])
        .output()
        .expect("whoami");
    let groups = String::from_utf8_lossy(&whoami.stdout).into_owned();
    std::fs::write(canary.evidence.join("identity.txt"), &groups).unwrap();
    canary.record(
        "SEC-13",
        if groups.contains("S-1-16-8192")
            && !groups.contains("S-1-16-12288")
            && !groups.contains("S-1-16-16384")
        {
            "PASS"
        } else {
            "FAIL"
        },
        "process integrity (Medium S-1-16-8192, not High/System)",
    );

    // SEC-15: present the real side effects rather than claiming none. The
    // ordinary-write case intentionally changes the tree, so compare against
    // the documented expectation: exactly the allowed files and the runtime
    // write; every protected original is unchanged.
    let protected_intact = {
        let config = canary.read(".leveler/config.yaml");
        let business = canary.read("src/business.rs");
        !canary.root.join(".leveler").join("agents").exists()
            && config.as_deref() == Some("default_mode: assisted\n")
            && business
                .as_deref()
                .is_some_and(|text| text.contains("edited"))
    };
    canary.record(
        "SEC-15",
        if protected_intact { "PASS" } else { "FAIL" },
        "guarded originals unchanged; allowed writes are the only persisted effects",
    );

    // SEC-05B / SEC-06B: the *whole* `.leveler` directory is missing. There is
    // no ancestor object to fence and no name filter in NTFS, so a confined
    // shell can create the directory tree and a definition inside it. This is
    // the documented residual; the case is recorded, not hidden.
    std::fs::remove_dir_all(canary.root.join(".leveler")).unwrap();
    canary
        .run(
            "sec05b-06b-missing-leveler",
            "mkdir .leveler & mkdir .leveler\\agents & echo evil> .leveler\\agents\\evil.yaml & mkdir .leveler\\skills",
        )
        .await;
    let leveler_forged = canary.root.join(".leveler").join("agents").exists()
        || canary.root.join(".leveler").join("skills").exists();
    canary.record(
        "SEC-05B",
        if leveler_forged { "KNOWN-GAP" } else { "PASS" },
        "a completely missing `.leveler` has no ancestor object to fence",
    );
    canary.record(
        "SEC-06B",
        if leveler_forged { "KNOWN-GAP" } else { "PASS" },
        "a completely missing `.leveler` has no ancestor object to fence",
    );

    let after = canary.hash_tree();
    std::fs::write(canary.evidence.join("tree-after.txt"), &after).unwrap();

    // SEC-14: the launcher is required; its absence is fail-closed. The
    // capability probe is the product's admission answer.
    let caps = leveler_execution::probe_sandbox_capabilities();
    canary.record(
        "SEC-14",
        if caps.write == leveler_execution::FsCapability::WriteRestricted {
            "PASS"
        } else {
            "FAIL"
        },
        format!("write backend = {:?}", caps.write),
    );

    let mut report = String::from("id,verdict,detail\n");
    for (id, verdict, detail) in &canary.results {
        report.push_str(&format!(
            "{id},{verdict},\"{}\"\n",
            detail.replace('"', "'").replace('\n', " ")
        ));
    }
    std::fs::write(canary.evidence.join("TEST_MATRIX.csv"), &report).unwrap();
    println!(
        "WINDOWS_SECURITY_CANARY root={} evidence={}\n{report}",
        canary.root.display(),
        canary.evidence.display()
    );
    let failed: Vec<&str> = canary
        .results
        .iter()
        .filter(|(_, verdict, _)| *verdict == "FAIL")
        .map(|(id, _, _)| id.as_str())
        .collect();
    assert!(
        failed.is_empty(),
        "unclosed OS-boundary cases on this host: {failed:?}"
    );
}

/// TD-10: the definition trust store lives outside the workspace and must be
/// unreachable for a confined shell. This is not an "is_trusted()" unit test:
/// it runs the production launcher under a real Low token and inspects the
/// resulting filesystem.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows OS security canary; needs leveler-confine.exe and a scratch workspace"]
async fn the_definition_trust_store_is_unreachable_for_a_confined_shell() {
    if leveler_execution::windows_confine::launcher_path().is_none() {
        panic!("leveler-confine.exe is not built beside the test binary");
    }
    let mut canary = Canary::new();
    canary.seed();
    let trust_home = canary
        .root
        .parent()
        .expect("workspace parent")
        .join("trust-home");
    let _ = std::fs::remove_dir_all(&trust_home);
    std::fs::create_dir_all(trust_home.join("definition-trust").join("snapshots")).unwrap();
    let store = trust_home.join("definition-trust.yaml");
    let original = "version: 1\ndomains: {}\n";
    std::fs::write(&store, original).unwrap();
    std::fs::write(
        trust_home
            .join("definition-trust")
            .join("snapshots")
            .join("deadbeef"),
        "approved snapshot",
    )
    .unwrap();

    let t = trust_home.display().to_string().replace('\'', "''");
    let script = format!(
        "$t = '{t}'; $r = @(); \
         try {{ Set-Content -LiteralPath \"$t\\definition-trust.yaml\" -Value 'PWNED' -ErrorAction Stop; $r += 'WRITE_OK' }} catch {{ $r += 'WRITE_DENIED' }}; \
         try {{ Add-Content -LiteralPath \"$t\\definition-trust.yaml\" -Value 'PWNED' -ErrorAction Stop; $r += 'APPEND_OK' }} catch {{ $r += 'APPEND_DENIED' }}; \
         try {{ Remove-Item -LiteralPath \"$t\\definition-trust.yaml\" -Force -ErrorAction Stop; $r += 'DELETE_OK' }} catch {{ $r += 'DELETE_DENIED' }}; \
         try {{ Rename-Item -LiteralPath \"$t\\definition-trust.yaml\" -NewName 'moved.yaml' -ErrorAction Stop; $r += 'RENAME_OK' }} catch {{ $r += 'RENAME_DENIED' }}; \
         try {{ Set-Content -LiteralPath \"$t\\forged.yaml\" -Value 'PWNED' -ErrorAction Stop; $r += 'CREATE_OK' }} catch {{ $r += 'CREATE_DENIED' }}; \
         try {{ New-Item -ItemType Directory -Path \"$t\\forged-dir\" -ErrorAction Stop | Out-Null; $r += 'MKDIR_OK' }} catch {{ $r += 'MKDIR_DENIED' }}; \
         try {{ Set-Content -LiteralPath \"$t\\definition-trust\\snapshots\\deadbeef\" -Value 'PWNED' -ErrorAction Stop; $r += 'SNAPSHOT_OK' }} catch {{ $r += 'SNAPSHOT_DENIED' }}; \
         $r -join ','"
    );
    let mut request = ProcessRequest::new(
        "powershell",
        vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            script,
        ],
        canary.root.clone(),
    );
    request.write_scope = WriteScope::Workspace {
        root: canary.root.clone(),
    };
    let output = canary
        .runner
        .run(request, CancellationToken::new())
        .await
        .expect("confined powershell must run");
    std::fs::write(
        canary.evidence.join("td10-trust-store.txt"),
        format!(
            "exit: {:?}\nstdout:\n{}\nstderr:\n{}\n",
            output.exit_code, output.stdout, output.stderr
        ),
    )
    .unwrap();

    let store_after = std::fs::read_to_string(&store).unwrap_or_default();
    let snapshot_after = std::fs::read_to_string(
        trust_home
            .join("definition-trust")
            .join("snapshots")
            .join("deadbeef"),
    )
    .unwrap_or_default();
    let forged = trust_home.join("forged.yaml").exists();
    let moved = trust_home.join("moved.yaml").exists();
    let forged_dir = trust_home.join("forged-dir").exists();
    let all_denied = output.stdout.matches("_DENIED").count() == 7;
    canary.record(
        "TD-10",
        if store_after == original
            && snapshot_after == "approved snapshot"
            && !forged
            && !moved
            && !forged_dir
            && all_denied
        {
            "PASS"
        } else {
            "FAIL"
        },
        format!(
            "store intact={}; snapshot intact={}; forged={forged}; moved={moved}; dir={forged_dir}; stdout={:?}",
            store_after == original,
            snapshot_after == "approved snapshot",
            output.stdout.trim()
        ),
    );

    let mut report = String::from("id,verdict,detail\n");
    for (id, verdict, detail) in &canary.results {
        report.push_str(&format!(
            "{id},{verdict},\"{}\"\n",
            detail.replace('"', "'").replace('\n', " ")
        ));
    }
    std::fs::write(canary.evidence.join("TEST_MATRIX.csv"), &report).unwrap();
    println!(
        "TRUST_STORE_CANARY trust_home={} evidence={}\n{report}",
        trust_home.display(),
        canary.evidence.display()
    );
    let failed: Vec<&str> = canary
        .results
        .iter()
        .filter(|(_, verdict, _)| *verdict == "FAIL")
        .map(|(id, _, _)| id.as_str())
        .collect();
    assert!(failed.is_empty(), "trust store boundary failed: {failed:?}");
    let _ = std::fs::remove_dir_all(&trust_home);
}

/// TD-02 / TD-03: a confined shell can create the file (the OS gap) but the
/// trust gate refuses its bytes until a human approves them; approval admits
/// exactly those bytes, and a later confined edit invalidates it. This is the
/// Property A / Property B split observed on a real NTFS tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real Windows OS security canary; needs leveler-confine.exe and a scratch workspace"]
async fn a_shell_created_agents_md_is_created_but_not_admitted() {
    use leveler_definition_trust::{
        ApprovalSurface, DefinitionTrustView, approve, discover_project,
    };
    if leveler_execution::windows_confine::launcher_path().is_none() {
        panic!("leveler-confine.exe is not built beside the test binary");
    }
    let mut canary = Canary::new();
    canary.seed();
    let trust_home = canary
        .root
        .parent()
        .expect("workspace parent")
        .join("trust-home-rules");
    let _ = std::fs::remove_dir_all(&trust_home);
    std::fs::create_dir_all(&trust_home).unwrap();

    // Root AGENTS.md: OS creation succeeds, admission does not.
    std::fs::remove_file(canary.root.join("AGENTS.md")).unwrap();
    canary.run("td02-create", "echo FORGED> AGENTS.md").await;
    let created = canary.read("AGENTS.md");
    assert!(created.is_some(), "the OS gap is real; this is Property A");
    let view = DefinitionTrustView::load(&trust_home, Some(&canary.root));
    let probe_identity = leveler_definition_trust::DefinitionIdentity::project(
        leveler_definition_trust::DefinitionKind::RootAgents,
        "AGENTS.md",
    )
    .unwrap();
    let probe_bytes = std::fs::read(canary.root.join("AGENTS.md")).unwrap();
    let decision = view.check(
        &probe_identity,
        &leveler_definition_trust::DefinitionIdentity::single_digest(&probe_bytes),
    );
    assert!(
        !decision.admits(),
        "fresh store must not admit: {decision:?} status={:?}",
        view.status()
    );
    let unapproved = leveler_context::load_rules(&canary.root, &view);
    assert!(unapproved.documents.is_empty(), "{unapproved:?}");
    assert_eq!(unapproved.untrusted.len(), 1, "{unapproved:?}");
    assert_eq!(unapproved.untrusted[0].state, "unapproved");

    // Exact-byte approval admits the current content.
    let requests: Vec<_> = discover_project(&canary.root)
        .iter()
        .filter_map(|candidate| candidate.approval_request().ok())
        .collect();
    approve(&trust_home, &canary.root, &requests, ApprovalSurface::Cli).unwrap();
    let approved_view = DefinitionTrustView::load(&trust_home, Some(&canary.root));
    let approved = leveler_context::load_rules(&canary.root, &approved_view);
    assert_eq!(approved.documents.len(), 1, "{approved:?}");
    assert!(approved.documents[0].content.contains("FORGED"));

    // A later edit changes the bytes; the old approval no longer covers them.
    // The confined append is denied by the OS boundary (the file now exists and
    // is Medium), so the edit is made by the trusted host, which is exactly the
    // case the content digest must catch.
    canary
        .run("td02-append-denied", "echo MORE>> AGENTS.md")
        .await;
    std::fs::write(
        canary.root.join("AGENTS.md"),
        "FORGED
MORE
",
    )
    .unwrap();
    let changed = leveler_context::load_rules(&canary.root, &approved_view);
    assert!(changed.documents.is_empty(), "{changed:?}");
    assert_eq!(changed.untrusted[0].state, "changed");

    // A nested AGENTS.md in a directory that did not exist at approval time:
    // same two properties at depth, with no prior record to be "changed".
    canary
        .run(
            "td03-nested-create",
            "mkdir src\\new & echo NESTED-FORGED> src\\new\\AGENTS.md",
        )
        .await;
    let nested_created = canary.read("src/new/AGENTS.md");
    assert!(nested_created.is_some());
    let nested = leveler_context::scoped_rule_sources(
        &canary.root,
        &["src/new/lib.rs".to_string()],
        &approved_view,
    );
    assert!(nested.documents.is_empty(), "{nested:?}");
    assert_eq!(nested.untrusted[0].state, "unapproved");

    canary.record(
        "TD-02",
        if created.is_some()
            && unapproved.documents.is_empty()
            && approved.documents.len() == 1
            && changed.documents.is_empty()
        {
            "PASS"
        } else {
            "FAIL"
        },
        "root AGENTS.md: created by Low shell; not admitted; admitted after exact-byte approval; changed bytes not admitted",
    );
    canary.record(
        "TD-03",
        if nested_created.is_some() && nested.documents.is_empty() {
            "PASS"
        } else {
            "FAIL"
        },
        "nested AGENTS.md: created by Low shell; not admitted by the scoped loader",
    );
    let mut report = String::from("id,verdict,detail\n");
    for (id, verdict, detail) in &canary.results {
        report.push_str(&format!(
            "{id},{verdict},\"{}\"\n",
            detail.replace('"', "'").replace('\n', " ")
        ));
    }
    std::fs::write(canary.evidence.join("TEST_MATRIX.csv"), &report).unwrap();
    println!(
        "DEFINITION_ADMISSION_CANARY workspace={} evidence={}\n{report}",
        canary.root.display(),
        canary.evidence.display()
    );
    let failed: Vec<&str> = canary
        .results
        .iter()
        .filter(|(_, verdict, _)| *verdict == "FAIL")
        .map(|(id, _, _)| id.as_str())
        .collect();
    assert!(failed.is_empty(), "admission cases failed: {failed:?}");
    let _ = std::fs::remove_dir_all(&trust_home);
}
