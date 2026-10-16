//! Regression tests for the release transaction itself.
//!
//! These drive the real `pipeline` with a scripted dogfood lab and a scripted
//! GitHub, so every failure path — a failing layer, a cancelled CI run, a second
//! dispatch, a corrupted state file, a crash between stages — is exercised
//! without touching the network or the product.
//!
//! What they prove: the orchestration. What they do not prove: that CodeLeveler
//! works. Only a real dogfood run does that, and these tests never replace it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::cli::dispatch_with;
use crate::exec::{CommandSpec, FakeHost, ProcessOut};
use crate::json::Json;
use crate::root::Env;
use crate::tests::{
    BASELINE, SHA_A, SHA_B, Tmp, args, point_config, process, text, write_lab, write_versions,
};

/// The commit the version bump creates. Everything after it qualifies this SHA.
const CANDIDATE: &str = SHA_B;
const RELEASE_RUN: &str = "eval/release/20260101-000000-bbbbbbbbbbbb";

fn release_workflow() -> String {
    [
        "name: Release",
        "on:",
        "  workflow_dispatch:",
        "jobs:",
        "  build:",
        "    strategy:",
        "      matrix:",
        "        include:",
        "          - os: ubuntu-latest",
        "            target: x86_64-unknown-linux-gnu",
        "          - os: macos-latest",
        "            target: aarch64-apple-darwin",
        "",
    ]
    .join("\n")
}

fn ci_workflow() -> String {
    ["name: CI", "on:", "  push:", "    branches: [main]", ""].join("\n")
}

/// Everything the scripted world can be asked to become.
struct Script {
    head: String,
    committed: bool,
    /// Layer statuses by layer name; anything absent is PASS.
    layer_status: Vec<(String, String)>,
    /// Layer check ids to omit, to prove a stage notices missing work.
    drop_check: Option<String>,
    gate_exit: i32,
    gate_decision: String,
    rc_exit: i32,
    rc_decision: String,
    cases: Vec<(String, String)>,
    ci: String,
    release_run: String,
    /// The remote tag exists at this commit (the workflow creates it last).
    tag_sha: Option<String>,
    /// The GitHub release exists.
    released: bool,
    release_view: String,
    release_url: String,
    /// How many of the expected artifact files exist on the release.
    assets_published: usize,
    bad_checksum: bool,
    branch_sha: Option<String>,
    push_fails: bool,
    block_report: bool,
    dispatch_count: u32,
    layer_invocations: u32,
    /// `ps` reports no such process, so a stale lock must be reclaimed.
    dead_owner: bool,
    /// `git status --porcelain` reports this when it is not empty.
    dirty: String,
    /// The version's tag exists on origin but not in the local clone, which is
    /// what a release workflow-created tag looks like before a fetch.
    origin_tags_only: bool,
    /// Layers this run refuses to write, to model a gate that named a run but
    /// produced no evidence for one of its layers.
    omit_layers: Vec<String>,
}

impl Script {
    fn new() -> Script {
        Script {
            head: SHA_A.to_string(),
            committed: false,
            layer_status: Vec::new(),
            drop_check: None,
            gate_exit: 0,
            gate_decision: "PASS".to_string(),
            rc_exit: 0,
            rc_decision: "PASS".to_string(),
            cases: vec![
                ("smoke".to_string(), "PASS".to_string()),
                ("core".to_string(), "PASS".to_string()),
            ],
            ci: "success".to_string(),
            release_run: "success".to_string(),
            tag_sha: None,
            released: false,
            release_view: "published".to_string(),
            release_url: "https://example.invalid/releases/tag/v1.0.9".to_string(),
            assets_published: 4,
            bad_checksum: false,
            branch_sha: None,
            push_fails: false,
            block_report: false,
            dispatch_count: 0,
            layer_invocations: 0,
            dead_owner: false,
            dirty: String::new(),
            origin_tags_only: false,
            omit_layers: Vec::new(),
        }
    }

    fn status(&self, layer: &str) -> String {
        self.layer_status
            .iter()
            .find(|(name, _)| name == layer)
            .map(|(_, status)| status.clone())
            .unwrap_or_else(|| "PASS".to_string())
    }
}

fn layer_document(script: &Script, layer: &str) -> String {
    let required: &[&str] = match layer {
        "L0" => &["fmt", "clippy", "release_build"],
        // `feature_coverage_inventory` is the coverage preflight: a real L1 run
        // always produces it, so the scripted one must too.
        "L1" => &[
            "workspace_tests",
            "prefix_cache_stability",
            "feature_coverage_inventory",
        ],
        "L2" => &["tui_tests"],
        _ => &[],
    };
    let mut checks = Vec::new();
    for id in required {
        let dropped = script.drop_check.as_deref() == Some(*id);
        checks.push(format!(
            "{{\"id\":\"{id}\",\"status\":\"{}\"}}",
            if dropped { "SKIPPED" } else { "PASS" }
        ));
    }
    if layer == "L3" {
        for (id, status) in &script.cases {
            checks.push(format!("{{\"id\":\"{id}\",\"status\":\"{status}\"}}"));
        }
    }
    let status = script.status(layer);
    let cases = if layer == "L3" {
        let rendered = script
            .cases
            .iter()
            .map(|(id, status)| format!("{{\"id\":\"{id}\",\"status\":\"{status}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(",\"cases\":[{rendered}]")
    } else {
        String::new()
    };
    format!(
        "{{\"status\":\"{status}\",\"checks\":[{}]{cases}}}",
        checks.join(",")
    )
}

fn published_assets(version: &str) -> Vec<String> {
    let mut names = vec![
        format!("leveler-v{version}-x86_64-unknown-linux-gnu.tar.gz"),
        format!("leveler-v{version}-aarch64-apple-darwin.tar.gz"),
    ];
    names.push(format!("{}.sha256", names[0].clone()));
    names.push(format!("{}.sha256", names[1].clone()));
    names
}

fn run_list(script: &Script) -> String {
    let ci = match script.ci.as_str() {
        "missing" => return "[]".to_string(),
        "in_progress" => ("in_progress", "null"),
        "failure" => ("completed", "\"failure\""),
        "cancelled" => ("completed", "\"cancelled\""),
        _ => ("completed", "\"success\""),
    };
    let mut runs = vec![format!(
        "{{\"databaseId\":111,\"workflowName\":\"CI\",\"event\":\"push\",\"headSha\":\"{CANDIDATE}\",\"status\":\"{}\",\"conclusion\":{},\"url\":\"https://example.invalid/ci\",\"createdAt\":\"2026-01-01T00:00:00Z\"}}",
        ci.0, ci.1
    )];
    if script.dispatch_count > 0 {
        let release = match script.release_run.as_str() {
            "in_progress" => ("in_progress", "null"),
            "failure" => ("completed", "\"failure\""),
            "queued" => ("queued", "null"),
            _ => ("completed", "\"success\""),
        };
        runs.push(format!(
            "{{\"databaseId\":222,\"workflowName\":\"Release\",\"event\":\"workflow_dispatch\",\"headSha\":\"{CANDIDATE}\",\"status\":\"{}\",\"conclusion\":{},\"url\":\"https://example.invalid/release\",\"createdAt\":\"2026-01-01T00:01:00Z\"}}",
            release.0, release.1
        ));
    }
    format!("[{}]", runs.join(","))
}

fn release_view_json(script: &Script) -> String {
    let version = "1.0.9";
    let draft = script.release_view == "draft";
    let assets: Vec<String> = published_assets(version)
        .into_iter()
        .take(script.assets_published)
        .map(|name| format!("{{\"name\":\"{name}\",\"state\":\"uploaded\",\"size\":10}}"))
        .collect();
    format!(
        "{{\"tagName\":\"v{version}\",\"isDraft\":{draft},\"isPrerelease\":false,\"url\":\"{}\",\"assets\":[{}]}}",
        script.release_url,
        assets.join(",")
    )
}

fn command_line(spec: &CommandSpec) -> String {
    format!("{} {}", spec.program, spec.args.join(" "))
}

/// A dogfood lab + GitHub that answers exactly what the scripted world says.
fn scripted(script: Arc<Mutex<Script>>, repo: PathBuf, lab: PathBuf) -> FakeHost {
    FakeHost::new(move |spec| {
        let mut script = script.lock().expect("script");
        let run_dir = lab.join(RELEASE_RUN);
        let rc_id = format!("eval/rc/20260101-000000-{}", &CANDIDATE[..12]);

        if spec.program == "ps" {
            if script.dead_owner {
                return Ok(ProcessOut {
                    status: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            // The lock owner is alive: these tests check the decision, not the
            // process table.
            return Ok(text(&format!(
                "{}\n",
                spec.args.get(1).cloned().unwrap_or_default()
            )));
        }
        if spec.program == "rustc" {
            return Ok(text("rustc 1.90.0 (1159e78c4 2025-09-14)\n"));
        }
        if spec.program == "cargo" {
            return Ok(text("{}"));
        }
        if spec.program == "git" {
            return git_response(&mut script, spec);
        }
        if spec.program == "gh" {
            return gh_response(&mut script, spec, &repo, &lab, &run_dir);
        }
        if spec.program == "python3" {
            let line = command_line(spec);
            if line.contains("rc_check.py") {
                let dir = lab.join(&rc_id);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join("rc.json"),
                    format!(
                        "{{\"decision\":{{\"decision\":\"{}\",\"exit_code\":{},\"blockers\":[]}},\"reuse\":[]}}",
                        script.rc_decision, script.rc_exit
                    ),
                )
                .unwrap();
                return Ok(process(script.rc_exit, &format!("written: {rc_id}/\n")));
            }
            if line.contains("--from-artifacts") {
                std::fs::write(
                    run_dir.join("release.json"),
                    format!(
                        "{{\"decision\":{{\"decision\":\"{}\",\"exit_code\":{},\"qualification_complete\":{},\"blockers\":[]}},\"layers\":{{}}}}",
                        script.gate_decision,
                        script.gate_exit,
                        script.gate_exit == 0 || script.gate_exit == 2
                    ),
                )
                .unwrap();
                return Ok(process(script.gate_exit, ""));
            }
            // One layer invocation: write the layers it asked for, then name the
            // run directory. This mirrors the real gate's contract exactly.
            let run_id = spec
                .args
                .windows(2)
                .find(|pair| pair[0] == "--run-id")
                .map(|pair| pair[1].clone());
            let run_dir = match run_id {
                Some(id) => lab.join("eval/release").join(id),
                None => lab.join(RELEASE_RUN),
            };
            let requested = spec
                .args
                .windows(2)
                .find(|pair| pair[0] == "--layers")
                .map(|pair| pair[1].clone())
                .unwrap_or_default();
            std::fs::create_dir_all(&run_dir).unwrap();
            script.layer_invocations += 1;
            for layer in requested.split(',') {
                if layer.is_empty() {
                    continue;
                }
                if script.omit_layers.iter().any(|name| name == layer) {
                    continue;
                }
                std::fs::write(
                    run_dir.join(format!("{layer}.json")),
                    layer_document(&script, layer),
                )
                .unwrap();
            }
            // The real gate names the run as a repo-relative path with `/`
            // separators (`eval/release/<id>`), and the pipeline reads exactly
            // that shape out of the gate's output. `Path::display()` would
            // spell it `eval\release\<id>` on Windows and name nothing, which
            // is a harness defect, not a gate the pipeline should guess at.
            let relative = run_dir
                .strip_prefix(&lab)
                .unwrap_or(&run_dir)
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let slowest = script.status("L0") != "PASS";
            let exit = if slowest || script.status("L1") != "PASS" || script.status("L2") != "PASS"
            {
                1
            } else {
                0
            };
            return Ok(process(exit, &format!("written: {relative}/\n")));
        }
        if spec.program == "tar" || spec.program == "unzip" {
            // The unpacked artifact holds a runnable binary; the version call is
            // scripted below.
            let dir = spec
                .args
                .windows(2)
                .find(|pair| pair[0] == "-C" || pair[0] == "-d")
                .map(|pair| pair[1].clone())
                .unwrap_or_else(|| "/tmp".to_string());
            let unpacked = PathBuf::from(dir).join("unpacked");
            std::fs::create_dir_all(&unpacked).unwrap();
            std::fs::write(unpacked.join("leveler"), "#!/bin/sh\n").unwrap();
            return Ok(text(""));
        }
        if spec.program.ends_with("leveler") {
            return Ok(text("leveler 1.0.9\n"));
        }
        panic!("unexpected command in the release pipeline: {spec:?}");
    })
}

fn git_response(script: &mut Script, spec: &CommandSpec) -> Result<ProcessOut, crate::error::Fail> {
    let first = spec.args.first().map(String::as_str).unwrap_or("");
    match first {
        "rev-parse" => {
            if spec.args.get(1).map(String::as_str) == Some("--abbrev-ref") {
                return Ok(text("main\n"));
            }
            Ok(text(&format!("{}\n", script.head)))
        }
        "status" => Ok(text(&script.dirty.clone())),
        "remote" => Ok(text("origin\n")),
        "tag" => {
            if script.origin_tags_only {
                Ok(text("v1.0.7\n"))
            } else {
                Ok(text("v1.0.8\n"))
            }
        }
        "ls-remote" => {
            if script.origin_tags_only {
                // No refspec: the whole tag list, the way the version guard asks.
                return Ok(text(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\trefs/tags/v1.0.8\n\
                     aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\trefs/tags/v1.0.8^{}\n\
                     bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\trefs/tags/v1.0.7\n",
                ));
            }
            let path = spec.args.get(2).cloned().unwrap_or_default();
            if path.starts_with("refs/tags/") {
                // The tag is created by the workflow, after the commit is
                // pushed: that ordering is what makes a tag a result, not a
                // request.
                if script.committed
                    && let Some(sha) = &script.tag_sha
                {
                    let tag = path.trim_start_matches("refs/tags/");
                    return Ok(text(&format!("{sha}\trefs/tags/{tag}\n")));
                }
                return Ok(text(""));
            }
            match &script.branch_sha {
                Some(sha) => Ok(text(&format!("{sha}\trefs/heads/main\n"))),
                None => Ok(text("")),
            }
        }
        "add" => Ok(text("")),
        "commit" => {
            script.committed = true;
            script.head = CANDIDATE.to_string();
            Ok(text(""))
        }
        "diff" | "log" => Ok(text("")),
        "cat-file" => Ok(text("")),
        "push" => {
            if script.push_fails {
                return Ok(ProcessOut {
                    status: 128,
                    stdout: String::new(),
                    stderr: "fatal: unable to access the remote: connection timed out\n"
                        .to_string(),
                });
            }
            script.branch_sha = Some(script.head.clone());
            Ok(text(""))
        }
        "rev-parse-sha" => Ok(text("")),
        other => panic!("unexpected git subcommand {other}"),
    }
}

fn gh_response(
    script: &mut Script,
    spec: &CommandSpec,
    repo: &Path,
    lab: &Path,
    run_dir: &Path,
) -> Result<ProcessOut, crate::error::Fail> {
    let group = spec.args.first().map(String::as_str).unwrap_or("");
    let sub = spec.args.get(1).map(String::as_str).unwrap_or("");
    match (group, sub) {
        ("release", "list") => Ok(text("v1.0.8\n")),
        ("release", "view") => {
            if script.committed && script.released && script.release_view != "missing" {
                Ok(text(&release_view_json(script)))
            } else {
                Ok(ProcessOut {
                    status: 1,
                    stdout: String::new(),
                    stderr: "release not found\n".to_string(),
                })
            }
        }
        ("release", "download") => {
            let tag = spec.args.get(2).cloned().unwrap_or_default();
            let version = tag.trim_start_matches('v').to_string();
            let dir = spec
                .args
                .windows(2)
                .find(|pair| pair[0] == "--dir")
                .map(|pair| pair[1].clone())
                .unwrap_or_else(|| "/tmp".to_string());
            let dir = PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            for (index, name) in published_assets(&version).into_iter().enumerate() {
                if index >= script.assets_published {
                    continue;
                }
                if name.ends_with(".sha256") {
                    continue;
                }
                let body = format!("artifact bytes for {name}\n");
                std::fs::write(dir.join(&name), &body).unwrap();
                let digest = if script.bad_checksum {
                    crate::hash::sha256_text("something else")
                } else {
                    crate::hash::sha256_text(&body)
                };
                std::fs::write(
                    dir.join(format!("{name}.sha256")),
                    format!("{digest}  {name}\n"),
                )
                .unwrap();
            }
            let _ = repo;
            Ok(text(""))
        }
        ("run", "list") => Ok(text(&run_list(script))),
        ("run", "view") => {
            if spec.args.iter().any(|arg| arg == "jobs") {
                return Ok(text(
                    "{\"jobs\":[{\"name\":\"build\",\"conclusion\":\"success\"}]}",
                ));
            }
            let id = spec.args.get(2).cloned().unwrap_or_default();
            let (status, conclusion) = if id == "111" {
                match script.ci.as_str() {
                    "in_progress" => ("in_progress", "null".to_string()),
                    "failure" => ("completed", "\"failure\"".to_string()),
                    "cancelled" => ("completed", "\"cancelled\"".to_string()),
                    _ => ("completed", "\"success\"".to_string()),
                }
            } else {
                match script.release_run.as_str() {
                    "in_progress" => ("in_progress", "null".to_string()),
                    "queued" => ("queued", "null".to_string()),
                    "failure" => ("completed", "\"failure\"".to_string()),
                    _ => {
                        // A successful release workflow is what creates the tag
                        // and publishes the release.
                        script.tag_sha = Some(CANDIDATE.to_string());
                        script.released = true;
                        script.release_view = "published".to_string();
                        ("completed", "\"success\"".to_string())
                    }
                }
            };
            let _ = lab;
            let _ = run_dir;
            Ok(text(&format!(
                "{{\"databaseId\":{id},\"status\":\"{status}\",\"conclusion\":{conclusion},\"headSha\":\"{CANDIDATE}\",\"url\":\"https://example.invalid/run/{id}\",\"event\":\"workflow_dispatch\",\"workflowName\":\"Release\",\"createdAt\":\"2026-01-01T00:00:00Z\"}}"
            )))
        }
        ("workflow", "run") => {
            script.dispatch_count += 1;
            Ok(text(""))
        }
        other => panic!("unexpected gh invocation {other:?}"),
    }
}

struct World {
    repo: PathBuf,
    lab: PathBuf,
    script: Arc<Mutex<Script>>,
    _repo_tmp: Tmp,
    _lab_tmp: Tmp,
}

impl World {
    fn new() -> World {
        let repo_tmp = Tmp::new();
        let lab_tmp = Tmp::new();
        let repo = repo_tmp.path.canonicalize().unwrap();
        let lab = lab_tmp.path.canonicalize().unwrap();
        write_versions(&repo, "1.0.8");
        write_lab(&lab, BASELINE);
        point_config(&repo, &lab, "example/model");
        std::fs::create_dir_all(repo.join(".github/workflows")).unwrap();
        std::fs::write(repo.join(".github/workflows/ci.yml"), ci_workflow()).unwrap();
        std::fs::write(
            repo.join(".github/workflows/release.yml"),
            release_workflow(),
        )
        .unwrap();
        let script = Arc::new(Mutex::new(Script::new()));
        World {
            repo,
            lab,
            script,
            _repo_tmp: repo_tmp,
            _lab_tmp: lab_tmp,
        }
    }

    fn host(&self) -> FakeHost {
        scripted(
            Arc::clone(&self.script),
            self.repo.clone(),
            self.lab.clone(),
        )
    }

    fn set<F: FnOnce(&mut Script)>(&self, change: F) {
        change(&mut self.script.lock().expect("script"));
    }

    fn read<T, F: FnOnce(&Script) -> T>(&self, read: F) -> T {
        read(&self.script.lock().expect("script"))
    }

    /// A publish that is expected to stop before stage 4.
    fn publish_to_stage_3(&self, extra: &[&str]) -> (i32, String) {
        let mut argv = args(&["publish", "--stop-after", "3"]);
        argv.extend(extra.iter().map(|value| value.to_string()));
        let host = self.host();
        let outcome = dispatch_with(&self.repo, &argv, &Env::default(), &host);
        (
            outcome.code,
            format!("{}{}", outcome.stdout, outcome.stderr),
        )
    }

    fn resume(&self, extra: &[&str]) -> (i32, String, Vec<CommandSpec>) {
        let mut argv = args(&["publish", "--resume"]);
        argv.extend(extra.iter().map(|value| value.to_string()));
        let host = self.host();
        let outcome = dispatch_with(&self.repo, &argv, &Env::default(), &host);
        let log = host_log(&host);
        (
            outcome.code,
            format!("{}{}", outcome.stdout, outcome.stderr),
            log,
        )
    }

    fn run(&self, argv: &[&str]) -> (i32, String, Vec<CommandSpec>) {
        let host = self.host();
        let outcome = dispatch_with(&self.repo, &args(argv), &Env::default(), &host);
        let log = host_log(&host);
        (
            outcome.code,
            format!("{}{}", outcome.stdout, outcome.stderr),
            log,
        )
    }

    fn state(&self) -> Json {
        let current = std::fs::read_to_string(self.repo.join(".dev/release/CURRENT")).unwrap();
        let id = current.trim();
        let text =
            std::fs::read_to_string(self.repo.join(".dev/release").join(id).join("state.json"))
                .unwrap();
        Json::parse(&text).unwrap()
    }

    fn report(&self) -> String {
        let current = std::fs::read_to_string(self.repo.join(".dev/release/CURRENT")).unwrap();
        let id = current.trim();
        std::fs::read_to_string(
            self.repo
                .join(".dev/release")
                .join(id)
                .join("RELEASE_REPORT.md"),
        )
        .unwrap()
    }
}

/// `FakeHost` records every command; the pipeline's own retry loop does not run
/// here because the scripted CI is already terminal.
fn host_log(host: &FakeHost) -> Vec<CommandSpec> {
    host.log()
}

fn pushed(log: &[CommandSpec]) -> bool {
    log.iter()
        .any(|spec| spec.program == "git" && spec.args.first().map(String::as_str) == Some("push"))
}

fn dispatched(log: &[CommandSpec]) -> usize {
    log.iter()
        .filter(|spec| {
            spec.program == "gh"
                && spec.args.first().map(String::as_str) == Some("workflow")
                && spec.args.get(1).map(String::as_str) == Some("run")
        })
        .count()
}

// ── the happy path ──────────────────────────────────────────────────────────

#[test]
fn a_clean_release_runs_all_four_stages_and_verifies_the_published_artifacts() {
    let world = World::new();
    let (code, rendered, log) = world.run(&["publish"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(rendered.contains("READY_TO_PUBLISH: PASS"), "{rendered}");
    assert!(
        rendered.contains("stage 4 ci-cd-publish: PASS"),
        "{rendered}"
    );
    // The version commit is pushed once, the workflow is dispatched once, and
    // the release is only declared after tag, release and assets check out.
    assert_eq!(
        dispatched(&log),
        1,
        "the workflow is dispatched exactly once"
    );
    assert!(pushed(&log), "the version commit is pushed");
    assert!(
        log.iter().any(|spec| spec.program == "gh"
            && spec.args.first().map(String::as_str) == Some("release")
            && spec.args.get(1).map(String::as_str) == Some("download")),
        "the published artifacts are downloaded and hashed"
    );
    let state = world.state();
    assert_eq!(state.path_str("status"), Some("PASS"));
    assert_eq!(state.path_str("remote.tag"), Some("v1.0.9"));
    assert_eq!(state.path_str("remote.tag_sha"), Some(CANDIDATE));
    assert_eq!(state.path_str("remote.ci_conclusion"), Some("SUCCESS"));
    assert_eq!(
        state.path_str("remote.workflow_run_conclusion"),
        Some("success")
    );
    assert_eq!(
        state
            .at("remote")
            .at("verified_assets")
            .as_arr()
            .map(|a| a.len()),
        Some(2)
    );
    for stage in ["1", "2", "3", "4"] {
        let record = state
            .at("stages")
            .as_arr()
            .unwrap()
            .iter()
            .find(|record| record.path_f64("stage") == Some(stage.parse::<f64>().unwrap()))
            .expect("stage record");
        assert_eq!(record.path_str("status"), Some("PASS"), "stage {stage}");
    }
    let report = world.report();
    for section in [
        "## 一、发布概况",
        "## 二、阶段结果",
        "## 三、功能",
        "## 四、性能",
        "## 五、与上一正式版本对比",
        "## 六、架构变化",
        "## 七、CI/CD",
        "## 八、失败恢复记录",
        "## 九、正式结论",
        "RELEASED",
    ] {
        assert!(report.contains(section), "report is missing {section}");
    }
    assert!(
        std::fs::read_to_string(world.lab.join(RELEASE_RUN).join("PIPELINE_REPORT.md")).is_ok(),
        "the report is archived next to the dogfood evidence"
    );
}

// ── failure stops the run where it happened ─────────────────────────────────

#[test]
fn a_build_failure_stops_at_stage_one_and_reaches_no_remote() {
    let world = World::new();
    world.set(|script| script.layer_status.push(("L0".into(), "FAIL".into())));
    let (code, rendered, log) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("FAILED at stage 1"), "{rendered}");
    assert!(rendered.contains("LAYER_FAILED"), "{rendered}");
    assert!(!pushed(&log), "a failing build must not push");
    assert_eq!(dispatched(&log), 0);
    assert_eq!(world.read(|script| script.layer_invocations), 1);
    assert_eq!(world.state().path_str("status"), Some("FAIL"));
    assert!(world.report().contains("FAILED"));
}

#[test]
fn a_test_failure_stops_at_stage_two() {
    let world = World::new();
    world.set(|script| script.layer_status.push(("L1".into(), "FAIL".into())));
    let (code, rendered, log) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("FAILED at stage 2"), "{rendered}");
    assert!(!pushed(&log));
    assert_eq!(world.read(|script| script.layer_invocations), 2);
    let state = world.state();
    assert_eq!(
        state.at("stages").index(0).unwrap().path_str("status"),
        Some("PASS"),
        "the stage that passed keeps its verdict"
    );
    assert_eq!(
        state.at("stages").index(1).unwrap().path_str("status"),
        Some("FAIL")
    );
}

#[test]
fn a_dogfood_failure_stops_at_stage_three_and_never_reaches_ci() {
    let world = World::new();
    world.set(|script| {
        script.layer_status.push(("L3".into(), "FAIL".into()));
        script.gate_exit = 1;
        script.gate_decision = "FAIL".into();
    });
    let (code, rendered, log) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("FAILED at stage 3"), "{rendered}");
    assert!(!pushed(&log));
    assert_eq!(dispatched(&log), 0);
}

#[test]
fn a_case_timeout_is_reported_with_the_case_that_timed_out() {
    let world = World::new();
    world.set(|script| {
        script.cases = vec![
            ("smoke".into(), "PASS".into()),
            ("core".into(), "TIMEOUT".into()),
        ];
    });
    let (code, rendered) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0, "{rendered}");
    let state = world.state();
    let cases = state.at("stages").index(2).unwrap().at("cases").clone();
    assert_eq!(cases.path_f64("total"), Some(2.0));
    assert_eq!(cases.path_f64("passed"), Some(1.0));
    // A TIMEOUT is not counted as a pass and is visible in the summary.
    assert_eq!(cases.path_f64("failed"), Some(0.0));
    assert!(cases.path_f64("passed").unwrap() < cases.path_f64("total").unwrap());
    assert!(world.report().contains("L3 cases: 2 total, 1 pass"));
}

#[test]
fn a_required_check_that_never_ran_is_not_a_passing_layer() {
    let world = World::new();
    world.set(|script| script.drop_check = Some("workspace_tests".into()));
    let (code, rendered, log) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(
        rendered.contains("REQUIRED_CHECK_MISSING") || rendered.contains("LAYER_FAILED"),
        "{rendered}"
    );
    assert!(!pushed(&log));
}

#[test]
fn a_removed_layer_artifact_invalidates_the_stage_that_recorded_it() {
    let world = World::new();
    let (code, rendered) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0, "{rendered}");
    let removed = world.lab.join(RELEASE_RUN).join("L2.json");
    assert!(removed.is_file());
    std::fs::remove_file(&removed).unwrap();
    let (code, rendered, _) = world.resume(&["--stop-after", "3"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(
        rendered.contains("stage 2 automated-tests: INVALIDATED"),
        "a stage whose evidence no longer matches must be re-earned: {rendered}"
    );
    assert!(
        !rendered.contains("stage 2 automated-tests: REUSED"),
        "{rendered}"
    );
    assert!(world.lab.join(RELEASE_RUN).join("L2.json").is_file());
}

#[test]
fn the_coverage_preflight_must_have_run_or_stage_two_fails() {
    // The preflight is what proves every feature is registered and mapped
    // before the later stages produce evidence. A layer that silently skipped it
    // would let a release ship with an unchecked registry.
    let world = World::new();
    world.set(|script| script.drop_check = Some("feature_coverage_inventory".into()));
    let (code, rendered, log) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("REQUIRED_CHECK_MISSING"), "{rendered}");
    assert!(
        rendered.contains("feature_coverage_inventory"),
        "{rendered}"
    );
    assert!(!pushed(&log));
}

#[test]
fn a_layer_the_gate_never_produced_fails_the_stage_instead_of_passing_it() {
    let world = World::new();
    let (code, rendered) = world.publish_to_stage_3(&["--stop-after", "1"]);
    assert_eq!(code, 0, "{rendered}");
    // The gate names its run directory but produces no L1.json this time.
    world.set(|script| script.omit_layers.push("L1".into()));
    let (code, rendered, _) = world.resume(&["--stop-after", "2"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("GATE_REPORT_MISMATCH"), "{rendered}");
    assert!(rendered.contains("FAILED at stage 2"), "{rendered}");
}

// ── resume ──────────────────────────────────────────────────────────────────

#[test]
fn a_publish_stopped_after_stage_three_resumes_at_stage_four_and_keeps_the_version() {
    let world = World::new();
    let (code, rendered) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0, "{rendered}");
    let version_before = std::fs::read_to_string(world.repo.join("Cargo.toml")).unwrap();
    assert!(version_before.contains("version = \"1.0.9\""));
    assert!(!world.repo.join(".dev/release/publish.lock").exists());

    let (code, rendered, log) = world.resume(&[]);
    assert_eq!(code, 0, "{rendered}");
    // Stages 1-3 are reused, not re-run: the dogfood gate is invoked once more
    // only for stage 4's own version guard, never for the layers.
    assert!(
        rendered.contains("stage 1 build-static: REUSED"),
        "{rendered}"
    );
    assert!(
        rendered.contains("stage 2 automated-tests: REUSED"),
        "{rendered}"
    );
    assert!(
        rendered.contains("stage 3 real-dogfood: REUSED"),
        "{rendered}"
    );
    assert_eq!(
        world.read(|script| script.layer_invocations),
        3,
        "only the first run's three stages"
    );
    assert_eq!(dispatched(&log), 1);
    assert_eq!(
        std::fs::read_to_string(world.repo.join("Cargo.toml")).unwrap(),
        version_before,
        "resume keeps the version"
    );
    let state = world.state();
    assert_eq!(state.path_str("release_id"), Some("v1.0.9"));
    assert_eq!(state.path_str("version"), Some("1.0.9"));
    assert_eq!(state.path_f64("resume_count"), Some(1.0));
    assert_eq!(state.path_str("status"), Some("PASS"));
}

#[test]
fn a_resumed_publish_reuses_a_completed_rc_run_and_its_own_release_run() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    // Take the remote away: the push did not happen yet, so stage 4 has to do it.
    world.set(|script| {
        script.branch_sha = None;
        script.committed = true;
    });
    let (code, rendered, log) = world.resume(&[]);
    assert_eq!(code, 0, "{rendered}");
    assert!(pushed(&log), "the resumed stage 4 pushes the frozen commit");
    let state = world.state();
    assert_eq!(
        state.path_str("rc_run_id"),
        Some("20260101-000000-bbbbbbbbbbbb")
    );
}

#[test]
fn a_new_candidate_invalidates_the_stages_whose_inputs_changed() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    // The developer fixes something: a new commit on top of the version commit.
    world.set(|script| {
        script.head = "cccccccccccccccccccccccccccccccccccccccc".into();
    });
    let (code, rendered, _) = world.resume(&["--stop-after", "3"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(
        rendered.contains("candidate moved"),
        "the candidate change is reported: {rendered}"
    );
    assert!(
        rendered.contains("stage 1 build-static: INVALIDATED")
            || rendered.contains("stage 1 build-static: PASS"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("stage 1 build-static: REUSED"),
        "a new SHA must re-earn stage 1: {rendered}"
    );
    let state = world.state();
    assert_eq!(state.path_f64("retry_count"), Some(1.0));
    assert_eq!(
        state.path_str("candidate_sha"),
        Some("cccccccccccccccccccccccccccccccccccccccc")
    );
    assert_eq!(state.at("changed_paths").as_arr().map(|a| a.len()), Some(0));
    let report = world.report();
    assert!(report.contains("候选历史"), "{report}");
}

#[test]
fn a_changed_gate_definition_invalidates_every_stage() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    // The lab's own harness changed: a changed test is not reusable evidence.
    std::fs::write(
        world.lab.join("eval/scripts/release_check.py"),
        "#!/usr/bin/env python3\n# changed gate\n",
    )
    .unwrap();
    let (code, rendered, _) = world.resume(&["--stop-after", "3"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(
        !rendered.contains("stage 1 build-static: REUSED"),
        "a changed definition must invalidate stage 1: {rendered}"
    );
    assert!(
        !rendered.contains("stage 2 automated-tests: REUSED"),
        "{rendered}"
    );
}

#[test]
fn a_crash_between_stages_is_recovered_by_resume() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    // A crash leaves the stage it was in marked RUNNING. Resume must re-run that
    // stage and keep the ones that already have evidence.
    let current = std::fs::read_to_string(world.repo.join(".dev/release/CURRENT")).unwrap();
    let state_path = world
        .repo
        .join(".dev/release")
        .join(current.trim())
        .join("state.json");
    let text = std::fs::read_to_string(&state_path).unwrap();
    let mut doc = Json::parse(&text).unwrap();
    if let Json::Obj(pairs) = &mut doc
        && let Some((_, Json::Arr(stages))) = pairs.iter_mut().find(|(k, _)| k == "stages")
    {
        {
            for stage in stages.iter_mut() {
                if let Json::Obj(fields) = stage {
                    if let Some((_, status)) = fields.iter_mut().find(|(k, _)| k == "status") {
                        *status = Json::str("PASS");
                    }
                    if let Some((_, value)) = fields.iter_mut().find(|(k, _)| k == "finished_at") {
                        *value = Json::Null;
                    }
                }
            }
            if let Some(Json::Obj(fields)) = stages.get_mut(2)
                && let Some((_, status)) = fields.iter_mut().find(|(k, _)| k == "status")
            {
                *status = Json::str("RUNNING");
            }
        }
    }
    std::fs::write(&state_path, doc.to_pretty()).unwrap();
    let (code, rendered, _) = world.resume(&["--stop-after", "3"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(
        rendered.contains("stage 1 build-static: REUSED"),
        "{rendered}"
    );
    assert!(
        rendered.contains("stage 2 automated-tests: REUSED"),
        "{rendered}"
    );
    assert!(
        rendered.contains("stage 3 real-dogfood: PASS"),
        "the interrupted stage runs again: {rendered}"
    );
}

#[test]
fn a_resumed_release_refuses_a_tree_that_declares_another_version() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    write_versions(&world.repo, "1.0.10");
    let (code, rendered, _) = world.resume(&[]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_VERSION_CHANGED"), "{rendered}");
}

#[test]
fn status_and_report_describe_the_release_in_progress() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    let (code, rendered, _) = world.run(&["publish", "--status"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(rendered.contains("release:       v1.0.9"), "{rendered}");
    assert!(rendered.contains("1 build-static"), "{rendered}");
    let (code, rendered, _) = world.run(&["publish", "--report"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(rendered.contains("RELEASE_REPORT.md"), "{rendered}");
    assert!(world.report().contains("## 九、正式结论"));
}

#[test]
fn status_says_so_when_nothing_is_in_progress() {
    let world = World::new();
    let (code, rendered, _) = world.run(&["publish", "--status"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(rendered.contains("no release in progress"), "{rendered}");
    let (code, rendered, _) = world.run(&["publish", "--resume"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_NOT_IN_PROGRESS"), "{rendered}");
}

// ── transaction integrity ───────────────────────────────────────────────────

#[test]
fn a_tag_that_only_origin_has_still_counts_as_released() {
    // The release workflow creates the tag remotely. A clone that has not
    // fetched it sees the version it just released as unreleased, and used to
    // refuse the next release with VERSION_MISMATCH until someone ran
    // `git fetch --tags` by hand.
    let world = World::new();
    world.set(|script| script.origin_tags_only = true);
    let (code, rendered, _) = world.run(&["publish", "--dry-run"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(rendered.contains("next:       1.0.9"), "{rendered}");
}

#[test]
fn a_second_publish_is_refused_while_one_is_in_progress() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    let (code, rendered, _) = world.run(&["publish", "--patch"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_IN_PROGRESS"), "{rendered}");
}

#[test]
fn a_concurrent_publish_is_refused_by_the_lock() {
    let world = World::new();
    std::fs::create_dir_all(world.repo.join(".dev/release")).unwrap();
    std::fs::write(
        world.repo.join(".dev/release/publish.lock"),
        "{\"pid\": 1, \"release_id\": \"v1.0.9\"}\n",
    )
    .unwrap();
    let (code, rendered, _) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_BUSY"), "{rendered}");
    assert!(
        !world.repo.join(".dev/release/CURRENT").exists(),
        "a refused publish must not open a transaction"
    );
}

#[test]
fn a_dead_owner_leaves_the_lock_reclaimable() {
    let world = World::new();
    std::fs::create_dir_all(world.repo.join(".dev/release")).unwrap();
    // pid 0 never exists, so the previous publish is definitively gone.
    std::fs::write(
        world.repo.join(".dev/release/publish.lock"),
        "{\"pid\": 0, \"release_id\": \"v1.0.9\"}\n",
    )
    .unwrap();
    world.set(|script| script.dead_owner = true);
    let (code, rendered) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0, "{rendered}");
    assert!(!rendered.contains("RELEASE_BUSY"), "{rendered}");
    assert!(
        !world.repo.join(".dev/release/publish.lock").exists(),
        "the reclaimed lock is released when the publish finishes"
    );
}

#[test]
fn a_corrupted_state_file_is_reported_instead_of_guessed_at() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    let current = std::fs::read_to_string(world.repo.join(".dev/release/CURRENT")).unwrap();
    let state_path = world
        .repo
        .join(".dev/release")
        .join(current.trim())
        .join("state.json");
    std::fs::write(&state_path, "{\"state_version\": 1, \"release_id\"").unwrap();
    let (code, rendered, _) = world.run(&["publish", "--status"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_STATE_CORRUPT"), "{rendered}");
}

#[test]
fn a_state_file_from_another_version_is_not_read_as_this_one() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    let current = std::fs::read_to_string(world.repo.join(".dev/release/CURRENT")).unwrap();
    let state_path = world
        .repo
        .join(".dev/release")
        .join(current.trim())
        .join("state.json");
    let text = std::fs::read_to_string(&state_path)
        .unwrap()
        .replace("\"state_version\": 1", "\"state_version\": 99");
    std::fs::write(&state_path, text).unwrap();
    let (code, rendered, _) = world.run(&["publish", "--status"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_STATE_VERSION"), "{rendered}");
}

#[test]
fn the_version_commit_is_created_before_qualification_and_never_hidden() {
    let world = World::new();
    world.set(|script| script.layer_status.push(("L0".into(), "FAIL".into())));
    let (code, rendered, _) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(
        rendered.contains("exists locally and was NOT tagged, pushed, or reset"),
        "{rendered}"
    );
    assert!(world.repo.join(".dev/release/CURRENT").exists());
    assert!(!world.repo.join(".dev/release/publish.lock").exists());
    assert_eq!(world.state().path_str("version_commit"), Some(CANDIDATE));
}

#[test]
fn a_failing_report_does_not_turn_a_failed_stage_into_a_passing_release() {
    let world = World::new();
    world.set(|script| {
        script.layer_status.push(("L0".into(), "FAIL".into()));
        script.block_report = true;
    });
    // A directory where the report must go makes the atomic replace fail.
    std::fs::create_dir_all(world.repo.join(".dev/release/v1.0.9/RELEASE_REPORT.md")).unwrap();
    let (code, rendered, _) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("LAYER_FAILED"), "{rendered}");
}

#[test]
fn a_dirty_worktree_is_refused_before_the_version_is_touched() {
    let world = World::new();
    let host = FakeHost::new(|spec| {
        if spec.program == "git" && spec.args.first().map(String::as_str) == Some("status") {
            return Ok(text(" M README.md\n"));
        }
        if spec.program == "git" {
            return Ok(text(&format!("{SHA_A}\n")));
        }
        Ok(text(""))
    });
    let outcome = dispatch_with(&world.repo, &args(&["publish"]), &Env::default(), &host);
    assert_ne!(outcome.code, 0);
    assert!(
        outcome.stderr.contains("WORKTREE_DIRTY"),
        "{}",
        outcome.stderr
    );
    assert!(
        std::fs::read_to_string(world.repo.join("Cargo.toml"))
            .unwrap()
            .contains("version = \"1.0.8\""),
        "a refused publish must not bump the version"
    );
}

// ── stage 4: CI and the remote ──────────────────────────────────────────────

#[test]
fn a_push_failure_stops_before_dispatch_and_can_be_resumed() {
    let world = World::new();
    world.set(|script| script.push_fails = true);
    let (code, rendered, log) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("PUSH_FAILED"), "{rendered}");
    assert_eq!(
        dispatched(&log),
        0,
        "nothing is dispatched after a failed push"
    );
    assert_eq!(world.state().path_f64("current_stage"), Some(4.0));
    // The network comes back.
    world.set(|script| script.push_fails = false);
    let (code, rendered, log) = world.resume(&[]);
    assert_eq!(code, 0, "{rendered}");
    assert!(pushed(&log));
    assert_eq!(dispatched(&log), 1);
}

#[test]
fn a_failing_required_ci_run_blocks_the_release() {
    let world = World::new();
    world.set(|script| script.ci = "failure".into());
    let (code, rendered, log) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("CI_FAILED"), "{rendered}");
    assert_eq!(
        dispatched(&log),
        0,
        "CI failure must not dispatch a release"
    );
    assert_eq!(
        world.state().path_str("remote.ci_conclusion"),
        Some("FAILURE")
    );
}

#[test]
fn a_cancelled_ci_run_is_not_a_pass() {
    let world = World::new();
    world.set(|script| script.ci = "cancelled".into());
    let (code, rendered, log) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("CI_FAILED"), "{rendered}");
    assert_eq!(dispatched(&log), 0);
    assert_eq!(
        world.state().path_str("remote.ci_conclusion"),
        Some("CANCELLED")
    );
}

#[test]
fn a_timed_out_ci_wait_can_be_resumed_without_redispatching() {
    let world = World::new();
    world.set(|script| script.ci = "in_progress".into());
    let (code, rendered, log) = world.run(&["publish", "--ci-timeout", "1", "--poll-seconds", "1"]);
    assert_ne!(code, 0);
    assert!(
        rendered.contains("CI_TIMEOUT") || rendered.contains("RELEASE_RUN_TIMEOUT"),
        "{rendered}"
    );
    assert!(
        !pushed(&log) || pushed(&log),
        "the push may or may not have happened"
    );
    world.set(|script| script.ci = "success".into());
    let (code, rendered, log) = world.resume(&[]);
    assert_eq!(code, 0, "{rendered}");
    assert_eq!(
        dispatched(&log),
        1,
        "the workflow is dispatched exactly once across attempts"
    );
}

#[test]
fn a_failed_release_run_is_not_silently_dispatched_again() {
    let world = World::new();
    world.set(|script| {
        script.release_run = "failure".into();
        script.release_view = "missing".into();
    });
    let (code, rendered, log) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(
        rendered.contains("RELEASE_RUN_FAILED") || rendered.contains("RELEASE_RUN_TIMEOUT"),
        "{rendered}"
    );
    assert_eq!(dispatched(&log), 1);
    // The same run is still there; without --retry-remote nothing new is asked for.
    let (code, rendered, log) = world.resume(&[]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_RUN_FAILED"), "{rendered}");
    assert_eq!(dispatched(&log), 0);
    // With the explicit flag a new attempt is dispatched.
    let (code, _rendered, log) = world.resume(&["--retry-remote"]);
    assert_ne!(code, 0, "the scripted run still fails");
    assert_eq!(
        dispatched(&log),
        1,
        "--retry-remote asks for exactly one more attempt"
    );
}

#[test]
fn an_in_flight_release_run_is_adopted_instead_of_redispatched() {
    let world = World::new();
    world.set(|script| {
        script.release_run = "in_progress".into();
        script.release_view = "draft".into();
    });
    let (code, rendered, log) = world.run(&["publish", "--ci-timeout", "1", "--poll-seconds", "1"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_RUN_TIMEOUT"), "{rendered}");
    assert_eq!(dispatched(&log), 1);
    // The workflow finishes: it creates the tag and publishes the release.
    world.set(|script| {
        script.release_run = "success".into();
        script.tag_sha = Some(CANDIDATE.to_string());
        script.released = true;
        script.release_view = "published".into();
    });
    let (code, rendered, log) = world.resume(&[]);
    assert_eq!(code, 0, "{rendered}");
    assert_eq!(
        dispatched(&log),
        0,
        "the run in flight is adopted, not duplicated"
    );
    assert_eq!(
        world.state().path_str("remote.workflow_run_status"),
        Some("completed")
    );
}

#[test]
fn a_tag_that_already_exists_at_another_commit_is_a_conflict() {
    let world = World::new();
    world.set(|script| {
        script.tag_sha = Some("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into());
    });
    let (code, rendered) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0, "{rendered}");
    let (code, rendered, log) = world.resume(&[]);
    assert_ne!(code, 0);
    assert!(rendered.contains("TAG_EXISTS"), "{rendered}");
    assert_eq!(dispatched(&log), 0);
}

#[test]
fn a_published_version_is_immutable() {
    let world = World::new();
    // The version already exists as a published release on the remote.
    world.set(|script| {
        script.committed = true;
        script.branch_sha = Some(SHA_B.to_string());
        script.tag_sha = Some(CANDIDATE.to_string());
        script.released = true;
    });
    let (code, rendered, _) = world.run(&["publish"]);
    assert_ne!(code, 0);
    assert!(
        rendered.contains("RELEASE_EXISTS") || rendered.contains("TAG_EXISTS"),
        "{rendered}"
    );
    assert!(
        std::fs::read_to_string(world.repo.join("Cargo.toml"))
            .unwrap()
            .contains("version = \"1.0.8\""),
        "a refused publish must not bump the version"
    );
}

#[test]
fn a_draft_release_is_resumed_rather_than_recreated() {
    let world = World::new();
    world.set(|script| {
        script.release_run = "in_progress".into();
        script.released = true;
        script.release_view = "draft".into();
        script.tag_sha = Some(CANDIDATE.to_string());
    });
    let (code, rendered) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0, "{rendered}");
    let (code, rendered, log) = world.resume(&["--ci-timeout", "1", "--poll-seconds", "1"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_RUN_TIMEOUT"), "{rendered}");
    assert_eq!(
        dispatched(&log),
        1,
        "the draft is resumed from the same run"
    );
    world.set(|script| {
        script.release_run = "success".into();
        script.release_view = "published".into();
    });
    let (code, rendered, _) = world.resume(&[]);
    assert_eq!(code, 0, "{rendered}");
    assert!(rendered.contains("READY_TO_PUBLISH: PASS"), "{rendered}");
}

#[test]
fn a_missing_published_asset_fails_the_release() {
    let world = World::new();
    world.set(|script| {
        script.dispatch_count = 1;
        script.release_run = "success".into();
        script.tag_sha = Some(CANDIDATE.to_string());
        script.released = true;
        script.release_view = "published".into();
        // Only one of the four expected files was published.
        script.assets_published = 1;
    });
    let (code, rendered) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0, "{rendered}");
    let (code, rendered, _) = world.resume(&[]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_ASSET_MISSING"), "{rendered}");
}

#[test]
fn a_checksum_that_does_not_match_the_downloaded_bytes_fails_the_release() {
    let world = World::new();
    world.set(|script| {
        script.dispatch_count = 1;
        script.release_run = "success".into();
        script.tag_sha = Some(CANDIDATE.to_string());
        script.released = true;
        script.release_view = "published".into();
        script.bad_checksum = true;
    });
    let (code, rendered) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0, "{rendered}");
    let (code, rendered, _) = world.resume(&[]);
    assert_ne!(code, 0);
    assert!(rendered.contains("RELEASE_CHECKSUM_MISMATCH"), "{rendered}");
}

#[test]
fn a_worktree_that_changed_after_qualification_is_not_published() {
    let world = World::new();
    let (code, _) = world.publish_to_stage_3(&[]);
    assert_eq!(code, 0);
    // An uncommitted edit after qualification: the frozen commit is no longer
    // what is on disk, so it must not be pushed or released.
    world.set(|script| script.dirty = " M crates/leveler-cli/src/cli.rs\n".into());
    let (code, rendered, log) = world.resume(&[]);
    assert_ne!(code, 0);
    assert!(rendered.contains("WORKTREE_DIRTY"), "{rendered}");
    assert!(!pushed(&log));
}

#[test]
fn the_push_names_the_qualified_sha_not_head() {
    let world = World::new();
    let (code, _rendered, log) = world.run(&["publish"]);
    assert_eq!(code, 0);
    let push = log
        .iter()
        .find(|spec| spec.program == "git" && spec.args.first().map(String::as_str) == Some("push"))
        .expect("the frozen commit is pushed");
    assert_eq!(
        push.args.get(2).map(String::as_str),
        Some(format!("{CANDIDATE}:refs/heads/main").as_str()),
        "the push source is the candidate SHA, never HEAD: {:?}",
        push.args
    );
}

#[test]
fn artifact_verification_can_be_skipped_but_the_report_says_so() {
    let world = World::new();
    let (code, rendered, _) = world.run(&["publish", "--no-artifact-verify"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(
        rendered.contains("the published checksums were NOT re-computed"),
        "{rendered}"
    );
    let report = world.report();
    assert!(report.contains("--no-artifact-verify"), "{report}");
    assert!(
        report.contains("UNVERIFIED"),
        "a release without artifact verification is not RELEASED: {report}"
    );
}

#[test]
fn the_expected_artifacts_come_from_the_release_workflow_matrix() {
    let world = World::new();
    let names = crate::pipeline::expected_artifacts(&world.repo, "1.0.9").unwrap();
    assert!(names.contains(&"leveler-v1.0.9-aarch64-apple-darwin.tar.gz".to_string()));
    assert!(names.contains(&"leveler-v1.0.9-aarch64-apple-darwin.tar.gz.sha256".to_string()));
    assert!(names.contains(&"leveler-v1.0.9-x86_64-unknown-linux-gnu.tar.gz".to_string()));
    assert_eq!(names.len(), 4);
    // A workflow with no matrix cannot be verified, and that is a failure, not
    // a silently empty check.
    std::fs::write(
        world.repo.join(".github/workflows/release.yml"),
        "name: Release\non:\n  workflow_dispatch:\n",
    )
    .unwrap();
    assert!(crate::pipeline::expected_artifacts(&world.repo, "1.0.9").is_err());
}

// ── the command surface ─────────────────────────────────────────────────────

#[test]
fn every_publish_mode_is_reachable_and_conflicting_flags_are_refused() {
    let world = World::new();
    let (code, rendered, _) = world.run(&["publish", "--status"]);
    assert_eq!(code, 0, "{rendered}");
    let (code, rendered, _) = world.run(&["publish", "--patch", "--resume"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("PUBLISH_MODE_CONFLICT"), "{rendered}");
    let (code, rendered, _) = world.run(&["publish", "--stop-after", "9"]);
    assert_eq!(code, 2, "{rendered}");
    assert!(rendered.contains("PUBLISH_STAGE_INVALID"), "{rendered}");
    let (code, rendered, _) = world.run(&["publish", "--release-scope", "web"]);
    assert_eq!(code, 2, "{rendered}");
    assert!(rendered.contains("PUBLISH_SCOPE_INVALID"), "{rendered}");
    let (code, rendered, _) = world.run(&["publish", "--patch", "--major"]);
    assert_ne!(code, 0);
    assert!(rendered.contains("PUBLISH_LEVEL_CONFLICT"), "{rendered}");
    let (code, rendered, _) = world.run(&["publish", "--nonsense"]);
    assert_eq!(code, 2, "{rendered}");
    assert!(rendered.contains("UNKNOWN_COMMAND"), "{rendered}");
}

#[test]
fn the_release_scope_is_passed_to_the_gate_and_recorded() {
    let world = World::new();
    let (code, rendered) = world.publish_to_stage_3(&["--release-scope", "cli-tui"]);
    assert_eq!(code, 0, "{rendered}");
    let _ = rendered;
    let state = world.state();
    assert_eq!(state.path_str("status"), Some("PENDING"));
    // The scope is part of the fingerprint, so changing it re-runs the stages.
    let (code, rendered, _) = world.resume(&["--stop-after", "3"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(
        !rendered.contains("stage 1 build-static: REUSED"),
        "a different scope is a different qualification: {rendered}"
    );
}

#[test]
fn release_qualifies_the_current_commit_without_bumping_or_pushing() {
    let world = World::new();
    let (code, rendered, log) = world.run(&["release"]);
    assert_eq!(code, 0, "{rendered}");
    assert!(rendered.contains("report:"), "{rendered}");
    assert!(!pushed(&log), "./dev release never pushes");
    assert_eq!(dispatched(&log), 0);
    assert_eq!(world.read(|script| script.layer_invocations), 3);
    assert!(
        std::fs::read_to_string(world.repo.join("Cargo.toml"))
            .unwrap()
            .contains("version = \"1.0.8\""),
        "./dev release does not bump the version"
    );
    assert!(
        !world.repo.join(".dev/release/CURRENT").exists(),
        "./dev release must not open a publish transaction"
    );
}
