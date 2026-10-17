//! The release report.
//!
//! Aggregates the layers that already exist — the release gate's own
//! `release.json`, the performance layer, the RC qualification, the feature
//! coverage gate — into one document per release, in Chinese, with real numbers
//! or an explicit `N/A` / `NOT_COMPARABLE`. No section invents a value.

use std::path::{Path, PathBuf};

use crate::error::Fail;
use crate::exec::{self, Host};
use crate::json::Json;
use crate::pipeline::Context;
use crate::state::{self, Status, Transaction};

#[derive(Clone, Debug, Default)]
pub struct Paths {
    pub markdown: PathBuf,
    pub json: PathBuf,
    pub archived: Option<PathBuf>,
    pub warnings: Vec<String>,
}

pub fn report_dir(repo: &Path, transaction: &Transaction) -> PathBuf {
    state::state_dir(repo, &transaction.release_id)
}

fn lab_run_dir(context: &Context, transaction: &Transaction) -> Option<PathBuf> {
    let run_id = transaction.dogfood_run_id.as_ref()?;
    let path = state::lab_run_dir(&context.lab.root, state::RELEASE_RUN_KIND, run_id);
    if path.is_dir() { Some(path) } else { None }
}

fn read_json(path: &Path) -> Option<Json> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| Json::parse(&text).ok())
}

fn lab_json(context: &Context, transaction: &Transaction, file: &str) -> Option<Json> {
    let run = lab_run_dir(context, transaction)?;
    read_json(&run.join(file))
}

/// Local-times the release so the report reads like a log, not a database dump.
pub fn write(context: &Context, transaction: &Transaction) -> Result<Paths, Fail> {
    let dir = report_dir(&context.repo, transaction);
    std::fs::create_dir_all(&dir).map_err(|err| {
        Fail::new(
            "RELEASE_REPORT_IO",
            format!("could not create {}: {err}", dir.display()),
        )
    })?;
    let mut warnings = Vec::new();
    let data = build_json(context, transaction);
    let json_path = dir.join("release.json");
    state::write_atomic(&json_path, &data.to_pretty())?;
    let markdown = build_markdown(context, transaction, &data);
    let markdown_path = dir.join("RELEASE_REPORT.md");
    state::write_atomic(&markdown_path, &markdown)?;
    let mut archived = None;
    if let Some(run) = lab_run_dir(context, transaction) {
        let target = run.join("PIPELINE_REPORT.md");
        match state::write_atomic(&target, &markdown) {
            Ok(()) => {
                archived = Some(target);
                let _ = state::write_atomic(&run.join("pipeline_release.json"), &data.to_pretty());
            }
            Err(err) => warnings.push(format!(
                "the report could not be archived next to the dogfood evidence: {}",
                err.detail
            )),
        }
    }
    Ok(Paths {
        markdown: markdown_path,
        json: json_path,
        archived,
        warnings,
    })
}

// ── machine-readable data ───────────────────────────────────────────────────

fn build_json(context: &Context, transaction: &Transaction) -> Json {
    let l4 = lab_json(context, transaction, "L4.json");
    let rc = transaction.rc_run_id.as_ref().and_then(|run| {
        read_json(&state::lab_run_dir(&context.lab.root, state::RC_RUN_KIND, run).join("rc.json"))
    });
    let gate = lab_json(context, transaction, "release.json");
    let coverage = feature_coverage(context, transaction);
    Json::obj(vec![
        ("kind", Json::str("codeleveler_release")),
        ("state", transaction.to_json()),
        (
            "dogfood",
            Json::obj(vec![
                ("run_id", optional(transaction.dogfood_run_id.as_deref())),
                ("gate", gate.unwrap_or(Json::Null)),
                ("performance", performance(l4.as_ref())),
            ]),
        ),
        ("rc", rc.unwrap_or(Json::Null)),
        ("feature_coverage", coverage.unwrap_or(Json::Null)),
        ("comparison", comparison(context, transaction)),
        ("changes", changes(context, transaction)),
        ("remote", transaction.remote.to_json()),
        ("conclusion", Json::str(conclusion(transaction).0)),
    ])
}

fn optional(value: Option<&str>) -> Json {
    match value {
        Some(text) => Json::str(text),
        None => Json::Null,
    }
}

fn feature_coverage(context: &Context, transaction: &Transaction) -> Option<Json> {
    let run = lab_run_dir(context, transaction)?;
    for name in ["feature_coverage.json", "feature_coverage.L3.json"] {
        if let Some(doc) = read_json(&run.join(name)) {
            return Some(doc);
        }
    }
    None
}

fn performance(l4: Option<&Json>) -> Json {
    let metrics = l4
        .map(|doc| doc.at("metrics").at("agent").clone())
        .unwrap_or(Json::Null);
    Json::obj(vec![
        ("metrics", metrics),
        (
            "runtime_profile",
            lab_runtime_profile(l4).unwrap_or(Json::Null),
        ),
        ("terminations", terminations(l4).unwrap_or(Json::Null)),
    ])
}

fn lab_runtime_profile(l4: Option<&Json>) -> Option<Json> {
    let doc = l4?;
    doc.get("runtime_profile").cloned()
}

fn terminations(l4: Option<&Json>) -> Option<Json> {
    let doc = l4?;
    doc.at("metrics").get("terminations").cloned()
}

// ── comparisons ─────────────────────────────────────────────────────────────

fn git(host: &dyn Host, repo: &Path, args: &[&str]) -> Result<String, Fail> {
    let out = host.run(&exec::git(repo, args))?;
    if out.status != 0 {
        return Err(Fail::new(
            "GIT_FAILED",
            format!(
                "git {} exited {}\n{}",
                args.join(" "),
                out.status,
                exec::first_lines(&out.stderr, 10)
            ),
        ));
    }
    Ok(out.stdout)
}

struct Changes {
    commits: Vec<String>,
    files: Vec<String>,
    /// Why the change set is unavailable, when it is.
    note: Option<String>,
}

fn collect_changes(context: &Context, transaction: &Transaction) -> Changes {
    let empty = |note: String| Changes {
        commits: Vec::new(),
        files: Vec::new(),
        note: Some(note),
    };
    let Some(previous) = &transaction.previous_release else {
        return empty("no previous release is recorded for this transaction".to_string());
    };
    // The tag for a release is created on origin by the release workflow, so a
    // checkout that has not fetched it cannot name the previous release. That is
    // a fact about this checkout, and the report says so instead of showing an
    // empty comparison as if the release had no changes.
    if git(
        context.host,
        &context.repo,
        &["rev-parse", "--verify", &format!("{previous}^{{commit}}")],
    )
    .is_err()
    {
        return empty(format!(
            "{previous} is not in this checkout, so the change set could not be \
             computed (run `git fetch --tags` and re-open the report)"
        ));
    }
    let range = format!("{previous}..{}", transaction.candidate_sha);
    let commits = git(
        context.host,
        &context.repo,
        &["log", "--no-merges", "--pretty=%s", &range],
    )
    .unwrap_or_default()
    .lines()
    .map(str::to_string)
    .collect();
    let files = git(
        context.host,
        &context.repo,
        &["diff", "--name-only", &range],
    )
    .unwrap_or_default()
    .lines()
    .map(str::to_string)
    .collect();
    Changes {
        commits,
        files,
        note: None,
    }
}

fn changes(context: &Context, transaction: &Transaction) -> Json {
    let collected = collect_changes(context, transaction);
    let mut features = Vec::new();
    let mut fixes = Vec::new();
    let mut others = Vec::new();
    for subject in &collected.commits {
        if subject.starts_with("feat") {
            features.push(subject.clone());
        } else if subject.starts_with("fix") {
            fixes.push(subject.clone());
        } else {
            others.push(subject.clone());
        }
    }
    let crates: Vec<String> = {
        let mut touched: Vec<String> = collected
            .files
            .iter()
            .filter_map(|path| path.split('/').nth(1).map(str::to_string))
            .filter(|segment| segment != "leveler-cli" || path_is_crate(&collected.files))
            .collect();
        touched.sort();
        touched.dedup();
        touched
    };
    let mut risky: Vec<String> = Vec::new();
    let risky_owners = [
        ("crates/leveler-protocol/", "protocol"),
        ("crates/leveler-client-protocol/", "protocol"),
        ("crates/leveler-remote-protocol/", "protocol"),
        ("crates/leveler-storage/", "persistence"),
        ("crates/leveler-execution/", "permission/execution"),
        ("crates/leveler-runtime-host/", "runtime lifecycle"),
        ("crates/leveler-lifecycle/", "lifecycle"),
        ("crates/leveler-permission", "permission"),
        ("Cargo.toml", "dependencies"),
        ("Cargo.lock", "dependencies"),
        (".github/workflows/", "ci"),
    ];
    for (prefix, owner) in risky_owners {
        if collected.files.iter().any(|path| path.starts_with(prefix))
            && !risky.contains(&owner.to_string())
        {
            risky.push(owner.to_string());
        }
    }
    Json::obj(vec![
        (
            "previous_release",
            optional(transaction.previous_release.as_deref()),
        ),
        ("commit_count", Json::num(collected.commits.len() as f64)),
        (
            "features",
            Json::Arr(features.iter().map(Json::str).collect()),
        ),
        ("fixes", Json::Arr(fixes.iter().map(Json::str).collect())),
        ("other", Json::Arr(others.iter().map(Json::str).collect())),
        ("changed_files", Json::num(collected.files.len() as f64)),
        (
            "changed_crates",
            Json::Arr(crates.iter().map(Json::str).collect()),
        ),
        (
            "owners_touched",
            Json::Arr(risky.iter().map(Json::str).collect()),
        ),
        ("note", optional(collected.note.as_deref())),
    ])
}

fn path_is_crate(files: &[String]) -> bool {
    files.iter().any(|path| path.starts_with("crates/"))
}

fn comparison(context: &Context, transaction: &Transaction) -> Json {
    let Some(baseline) = &transaction.baseline_sha else {
        return Json::obj(vec![("status", Json::str("NOT_COMPARABLE"))]);
    };
    let previous = previous_run_metrics(context, baseline);
    let current = lab_json(context, transaction, "L4.json");
    let Some(current) = current else {
        return Json::obj(vec![
            ("status", Json::str("NOT_COMPARABLE")),
            (
                "reason",
                Json::str("this release has no L4 performance evidence"),
            ),
        ]);
    };
    let Some(previous) = previous else {
        return Json::obj(vec![
            ("status", Json::str("NOT_COMPARABLE")),
            ("baseline_sha", Json::str(baseline.clone())),
            (
                "reason",
                Json::str("no L4 evidence was found for the dogfood baseline in this lab"),
            ),
        ]);
    };
    let mut rows = Vec::new();
    for metric in [
        "wall_seconds_p50",
        "rounds_p50",
        "total_tokens_p50",
        "tool_calls_p50",
        "ttff_ms_p50",
    ] {
        let from = previous.at("metrics").at("agent").path_f64(metric);
        let to = current.at("metrics").at("agent").path_f64(metric);
        let ratio = match (from, to) {
            (Some(from), Some(to)) if from > 0.0 => Some(to / from),
            _ => None,
        };
        rows.push(Json::obj(vec![
            ("metric", Json::str(metric)),
            ("previous", from.map(Json::num).unwrap_or(Json::Null)),
            ("candidate", to.map(Json::num).unwrap_or(Json::Null)),
            ("ratio", ratio.map(Json::num).unwrap_or(Json::Null)),
        ]));
    }
    let comparable = previous
        .at("environment")
        .path_str("case_set_hash")
        .zip(current.at("environment").path_str("case_set_hash"))
        .map(|(a, b)| a == b)
        .unwrap_or(false);
    Json::obj(vec![
        (
            "status",
            Json::str(if comparable {
                "COMPARABLE"
            } else {
                "NOT_COMPARABLE"
            }),
        ),
        ("baseline_sha", Json::str(baseline.clone())),
        ("metrics", Json::Arr(rows)),
        (
            "note",
            Json::str(
                "the baseline is the dogfood release baseline (eval/release/BASELINE), \
                 not the previous public release; the two are different questions",
            ),
        ),
    ])
}

fn previous_run_metrics(context: &Context, baseline: &str) -> Option<Json> {
    let short = state::short_sha(baseline);
    let root = context.lab.root.join("eval/release");
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(&short) && path.join("L4.json").is_file())
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    read_json(&candidates.pop()?.join("L4.json"))
}

// ── markdown ────────────────────────────────────────────────────────────────

fn build_markdown(context: &Context, transaction: &Transaction, data: &Json) -> String {
    let mut out = String::new();
    let (verdict, verdict_note) = conclusion(transaction);
    out.push_str(&format!(
        "# CodeLeveler {} 发布报告\n\n",
        transaction.version
    ));
    out.push_str(
        "> 由 `./dev publish` 生成。数据来自本次 release transaction 与 dogfood/RC 证据，未测量的值标记 `N/A`。\n\n",
    );

    // 一、发布概况
    out.push_str("## 一、发布概况\n\n");
    out.push_str("| 项 | 值 |\n| --- | --- |\n");
    out.push_str(&format!("| 版本 | {} |\n", transaction.version));
    out.push_str(&format!("| Release ID | `{}` |\n", transaction.release_id));
    out.push_str(&format!("| 候选 SHA | `{}` |\n", transaction.candidate_sha));
    if transaction.candidate_history.len() > 1 {
        out.push_str(&format!(
            "| 候选历史 | {} |\n",
            transaction
                .candidate_history
                .iter()
                .map(|sha| format!("`{}`", state::short_sha(sha)))
                .collect::<Vec<_>>()
                .join(" → ")
        ));
    }
    out.push_str(&format!(
        "| 开始时间 | {} |\n",
        crate::clock::iso8601(transaction.started_at)
    ));
    if let Some(finished) = transaction.finished_at {
        out.push_str(&format!(
            "| 结束时间 | {} |\n",
            crate::clock::iso8601(finished)
        ));
    }
    out.push_str(&format!(
        "| 总耗时 | {} |\n",
        crate::clock::human_duration(transaction.duration())
    ));
    out.push_str(&format!("| 最终状态 | {} |\n", transaction.status.label()));
    out.push_str(&format!("| 正式结论 | **{verdict}** |\n"));
    out.push_str(&format!(
        "| 上一次正式发布 | {} |\n",
        transaction
            .previous_release
            .clone()
            .unwrap_or_else(|| "N/A".to_string())
    ));
    out.push_str(&format!(
        "| Dogfood 有效基线 | `{}` |\n",
        transaction
            .baseline_sha
            .clone()
            .unwrap_or_else(|| "N/A".to_string())
    ));
    out.push_str(&format!(
        "| 模型 | {} |\n",
        transaction
            .model
            .clone()
            .unwrap_or_else(|| "N/A".to_string())
    ));
    out.push_str(&format!("| 重试次数 | {} |\n", transaction.retry_count));
    out.push_str(&format!("| resume 次数 | {} |\n", transaction.resume_count));
    out.push('\n');
    out.push_str(&format!("{verdict_note}\n\n"));
    if !context.opts.verify_artifacts {
        out.push_str(
            "> 注意：本次运行使用 `--no-artifact-verify`，公开制品的校验和未被重新计算。\n\n",
        );
    }

    // 二、阶段结果
    out.push_str("## 二、阶段结果\n\n");
    out.push_str(
        "| Stage | 状态 | 耗时 | 复用 | 证据 | 失败原因 |\n| --- | --- | --- | --- | --- | --- |\n",
    );
    for record in &transaction.stages {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            state::stage_title(record.stage),
            record.status.label(),
            if record.seconds > 0.0 {
                crate::clock::human_duration(record.seconds)
            } else {
                "N/A".to_string()
            },
            if record.reused { "是" } else { "否" },
            record
                .evidence_hash
                .as_ref()
                .map(|hash| format!("`{}…`", &hash[..16.min(hash.len())]))
                .unwrap_or_else(|| "N/A".to_string()),
            record
                .failure_reason
                .clone()
                .unwrap_or_else(|| "-".to_string())
        ));
    }
    out.push('\n');
    if let Some(run) = &transaction.dogfood_run_id {
        out.push_str(&format!("- Dogfood run: `{run}`\n"));
    }
    if let Some(run) = &transaction.rc_run_id {
        out.push_str(&format!("- RC run: `{run}`\n"));
    }
    for record in &transaction.stages {
        for line in &record.detail {
            out.push_str(&format!("- stage {}: {line}\n", record.stage));
        }
    }
    out.push('\n');
    let dir = report_dir(&context.repo, transaction);
    out.push_str(&format!("证据目录：`{}`\n\n", dir.display()));

    // 三、功能
    out.push_str("## 三、功能\n\n");
    let changes = changes(context, transaction);
    let features = changes.at("features").as_arr().unwrap_or(&[]);
    let fixes = changes.at("fixes").as_arr().unwrap_or(&[]);
    out.push_str(&format!(
        "对比范围：`{}` → `{}`，共 {} 个提交、{} 个文件变更。\n\n",
        transaction
            .previous_release
            .clone()
            .unwrap_or_else(|| "N/A".to_string()),
        state::short_sha(&transaction.candidate_sha),
        changes.at("commit_count").as_f64().unwrap_or(0.0) as i64,
        changes.at("changed_files").as_f64().unwrap_or(0.0) as i64
    ));
    if let Some(note) = changes.at("note").as_str() {
        out.push_str(&format!("> 变更集不可用：{note}\n\n"));
    }
    out.push_str(&format!("### 新增功能（{}）\n\n", features.len()));
    push_list(
        &mut out,
        features,
        "- `{}`\n",
        "- 未识别到 `feat:` 提交；行为变化见 Bug 修复与行为变化一节。\n",
    );
    out.push_str(&format!("\n### Bug 修复（{}）\n\n", fixes.len()));
    push_list(&mut out, fixes, "- `{}`\n", "- 未识别到 `fix:` 提交。\n");
    out.push_str("\n### 行为变化与其它提交\n\n");
    push_list(
        &mut out,
        changes.at("other").as_arr().unwrap_or(&[]),
        "- `{}`\n",
        "- 无。\n",
    );
    out.push('\n');
    out.push_str("### 实际 Dogfood 覆盖\n\n");
    if let Some(doc) = lab_json(context, transaction, "L3.json") {
        let cases = doc.at("cases").as_arr().unwrap_or(&[]);
        out.push_str(&format!(
            "本次 L3 真实模型 dogfood 共 {} 个 case：\n\n",
            cases.len()
        ));
        let mut suites: Vec<(String, i64, i64)> = Vec::new();
        for case in cases {
            let suite = case
                .at("suite")
                .as_str()
                .or_else(|| case.at("lane").as_str())
                .unwrap_or("unspecified")
                .to_string();
            // L3 cases carry `passed` (the run's own verdict). `expect_passed`
            // is the case's declared expectation, which is not the same thing:
            // reading a field the document does not have reported 0 PASS for a
            // 19/19 run.
            let passed = case
                .at("passed")
                .as_bool()
                .or_else(|| case.at("expect_passed").as_bool())
                .unwrap_or(false);
            match suites.iter_mut().find(|(name, _, _)| name == &suite) {
                Some((_, total, ok)) => {
                    *total += 1;
                    if passed {
                        *ok += 1;
                    }
                }
                None => suites.push((suite, 1, if passed { 1 } else { 0 })),
            }
        }
        out.push_str("| suite | case 数 | PASS |\n| --- | --- | --- |\n");
        for (suite, total, ok) in &suites {
            out.push_str(&format!("| {suite} | {total} | {ok} |\n"));
        }
    } else {
        out.push_str("N/A —— 本次没有 L3 证据。\n");
    }
    out.push_str("\n### 未验证功能\n\n");
    match feature_coverage(context, transaction) {
        Some(coverage) => push_list(
            &mut out,
            coverage.at("unverified_list").as_arr().unwrap_or(&[]),
            "- `{}`\n",
            "- 无（coverage gate 未列出）。\n",
        ),
        None => out.push_str("- `N/A` —— 本次运行没有 feature coverage 证据。\n"),
    }
    out.push('\n');
    write_feature_coverage(&mut out, context, transaction);
    write_performance(&mut out, context, transaction, data);
    write_comparison(&mut out, data);
    write_architecture(&mut out, context, transaction);
    write_ci(&mut out, transaction);
    write_recovery(&mut out, transaction);
    write_conclusion(&mut out, transaction, &verdict, &verdict_note);
    out
}

fn push_list(out: &mut String, items: &[Json], format: &str, empty: &str) {
    if items.is_empty() {
        out.push_str(empty);
        return;
    }
    for item in items {
        if let Some(text) = item.as_str() {
            out.push_str(&format.replace("{}", text));
        }
    }
}

fn write_feature_coverage(out: &mut String, context: &Context, transaction: &Transaction) {
    out.push_str("## 三之补、FEATURE COVERAGE REPORT\n\n");
    let Some(coverage) = feature_coverage(context, transaction) else {
        out.push_str(
            "N/A —— 本次运行没有生成 feature coverage 报告。正式发布要求该报告存在；\
             缺失本身就是覆盖缺口。\n\n",
        );
        return;
    };
    let summary = coverage.at("summary");
    let rc = coverage.at("rc_evidence");
    let number = |key: &str| summary.at(key).as_f64().unwrap_or(0.0) as i64;
    out.push_str(&format!(
        "- 报告来源：`{}`\n- 覆盖判定：**{}**\n- 审计阶段：**{}**\n- RC 证据引用：**{}**\n\n",
        coverage
            .at("report_path")
            .as_str()
            .unwrap_or("eval/release/<run>/feature_coverage.json"),
        coverage.at("status").as_str().unwrap_or("UNKNOWN"),
        // `final` is the only phase that may report a feature as covered on RC
        // evidence; a final verdict taken at `pre_rc` is the defect this field
        // exists to make visible.
        coverage.at("phase").as_str().unwrap_or("UNKNOWN"),
        rc.at("status").as_str().unwrap_or("absent"),
    ));
    if let Some(reason) = rc.at("detail").as_str()
        && !reason.is_empty()
    {
        out.push_str(&format!("- RC 引用说明：{reason}\n"));
        if let Some(run) = rc.at("rc_run_id").as_str() {
            out.push_str(&format!("- RC run：`{run}`\n"));
        }
        if let Some(digest) = rc.at("rc_json_sha256").as_str() {
            let short: String = digest.chars().take(16).collect();
            out.push_str(&format!("- rc.json sha256：`{short}`\n"));
        }
        out.push('\n');
    }
    out.push_str("| 指标 | 数量 |\n| --- | --- |\n");
    for (label, key) in [
        ("正式功能总数", "total"),
        ("已覆盖 COVERED", "covered"),
        ("部分覆盖 PARTIAL", "partial"),
        ("未覆盖 UNCOVERED", "uncovered"),
        ("未验证 UNVERIFIED", "unverified"),
        ("失败 FAILED", "failed"),
        ("超出范围 OUT_OF_SCOPE", "out_of_scope"),
        ("不支持 UNSUPPORTED", "unsupported"),
        ("核心功能总数", "core_total"),
        ("核心已覆盖", "core_covered"),
        ("核心未覆盖", "core_gaps"),
    ] {
        out.push_str(&format!("| {label} | {} |\n", number(key)));
    }
    out.push('\n');
    // Four denominators, stated so the numbers cannot be read as one another.
    // A coverage artifact written before `coverage_core_percent` existed has no
    // such key; derive it rather than print a misleading 0.0%.
    let percent = |key: &str| summary.at(key).as_f64().unwrap_or(0.0);
    let core_percent = {
        let recorded = percent("coverage_core_percent");
        if recorded > 0.0 {
            recorded
        } else {
            let total = number("core_total");
            if total > 0 {
                100.0 * number("core_covered") as f64 / total as f64
            } else {
                0.0
            }
        }
    };
    out.push_str(&format!(
        "| 覆盖率口径 | 数值 | 定义 |\n| --- | --- | --- |\n\
         | 核心功能覆盖率 | {:.1}% | 核心 COVERED / 核心总数（{}/{}）——**发布阻断指标** |\n\
         | 本次候选有效证据覆盖率 | {:.1}% | 全部 COVERED / 全部功能（{}/{}），分母含 product/extended/out_of_scope |\n\
         | 功能映射覆盖率 | {:.1}% | 有任意证据引用的功能 / 全部功能 |\n\
         | 真实 E2E 覆盖率 | {:.1}% | 证据含 pty/eval/rc/l2 的功能 / 全部功能 |\n\n",
        core_percent,
        number("core_covered"),
        number("core_total"),
        percent("coverage_candidate_evidence_percent"),
        number("covered"),
        number("total"),
        percent("coverage_mapping_percent"),
        percent("coverage_e2e_percent"),
    ));
    let list = |out: &mut String, title: &str, key: &str| {
        let items = coverage.at(key).as_arr().unwrap_or(&[]);
        out.push_str(&format!("### {title}（{}）\n\n", items.len()));
        if items.is_empty() {
            out.push_str("- 无。\n\n");
            return;
        }
        for item in items {
            let id = item.at("feature_id").as_str().unwrap_or("?");
            let name = item.at("feature_name").as_str().unwrap_or("");
            let reason = item
                .at("reason")
                .as_str()
                .map(|text| format!(" —— {text}"))
                .unwrap_or_default();
            out.push_str(&format!("- `{id}` {name}{reason}\n"));
        }
        out.push('\n');
    };
    list(out, "核心未覆盖功能", "core_uncovered_list");
    list(out, "未覆盖功能", "uncovered_list");
    list(out, "未验证功能", "unverified_list");
    list(out, "本次新增但未登记的功能", "drift_new");
    list(out, "覆盖率下降", "regressions");
    list(out, "与上一正式版本的覆盖变化", "delta");
}

fn write_performance(out: &mut String, context: &Context, transaction: &Transaction, data: &Json) {
    out.push_str("## 四、性能\n\n");
    let metrics = data.at("dogfood").at("performance").at("metrics");
    let number = |key: &str| metrics.path_f64(key);
    out.push_str("| 指标 | 值 |\n| --- | --- |\n");
    let row = |out: &mut String, label: &str, value: Option<f64>, unit: &str| {
        out.push_str(&format!(
            "| {label} | {} |\n",
            value
                .map(|value| format!("{value:.2}{unit}"))
                .unwrap_or_else(|| "N/A".to_string())
        ));
    };
    row(out, "首次响应时间 ttff p50", number("ttff_ms_p50"), " ms");
    row(out, "单 case 墙钟 p50", number("wall_seconds_p50"), " s");
    row(out, "执行轮数 p50", number("rounds_p50"), "");
    row(out, "工具调用 p50", number("tool_calls_p50"), "");
    row(out, "输入 token p50", number("input_tokens_p50"), "");
    row(out, "输出 token p50", number("output_tokens_p50"), "");
    row(out, "总 token p50", number("total_tokens_p50"), "");
    row(out, "模型成本 p50", number("cost_usd_p50"), " USD");
    if let Some(profile) = data.at("dogfood").at("performance").get("runtime_profile") {
        for (label, key) in [
            ("编译/构建阶段 L0", "L0"),
            ("自动化测试阶段 L1", "L1"),
            ("TUI/PTY 阶段 L2", "L2"),
            ("真实模型 dogfood L3", "L3"),
            ("性能收集 L4", "L4"),
        ] {
            row(out, label, profile.path_f64(key), " s");
        }
        row(out, "dogfood 总耗时", profile.path_f64("wall"), " s");
    }
    out.push('\n');
    let cases = lab_json(context, transaction, "L3.json")
        .map(|doc| doc.at("cases").as_arr().unwrap_or(&[]).len())
        .unwrap_or(0);
    let passed = lab_json(context, transaction, "L3.json")
        .map(|doc| {
            doc.at("cases")
                .as_arr()
                .unwrap_or(&[])
                .iter()
                .filter(|case| case.at("status").as_str() == Some("PASS"))
                .count()
        })
        .unwrap_or(0);
    if cases > 0 {
        out.push_str(&format!(
            "- 任务完成率（L3）：{passed}/{cases} = {:.1}%\n",
            100.0 * passed as f64 / cases as f64
        ));
    } else {
        out.push_str("- 任务完成率（L3）：N/A\n");
    }
    let false_completions = lab_json(context, transaction, "L3.json")
        .map(|doc| {
            doc.at("cases")
                .as_arr()
                .unwrap_or(&[])
                .iter()
                .filter(|case| {
                    case.at("false_completion").as_bool().unwrap_or(false)
                        || case.at("signals").path_str("false_completion").is_some()
                })
                .count()
        })
        .unwrap_or(0);
    out.push_str(&format!(
        "- False Completion（L3 信号）：{}\n",
        if cases > 0 {
            false_completions.to_string()
        } else {
            "N/A".to_string()
        }
    ));
    out.push_str("- 内存 / CPU：N/A —— 本次流水线未采集这两个指标（未测量，不是 0）。\n");
    out.push('\n');
}

fn write_comparison(out: &mut String, data: &Json) {
    out.push_str("## 五、与上一正式版本对比\n\n");
    let comparison = data.at("comparison");
    out.push_str(&format!(
        "基线：`{}`（dogfood 性能基线，用于实验与性能资格判定）\n\n",
        comparison.at("baseline_sha").as_str().unwrap_or("N/A")
    ));
    out.push_str(&format!(
        "状态：**{}**\n\n",
        comparison.at("status").as_str().unwrap_or("NOT_COMPARABLE")
    ));
    if let Some(reason) = comparison.at("reason").as_str() {
        out.push_str(&format!("原因：{reason}\n\n"));
    }
    let metrics = comparison.at("metrics").as_arr().unwrap_or(&[]);
    if !metrics.is_empty() {
        out.push_str("| 指标 | 基线 | 候选 | 比值 |\n| --- | --- | --- | --- |\n");
        for row in metrics {
            let render = |key: &str| {
                row.at(key)
                    .as_f64()
                    .map(|value| format!("{value:.2}"))
                    .unwrap_or_else(|| "N/A".to_string())
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                row.at("metric").as_str().unwrap_or("?"),
                render("previous"),
                render("candidate"),
                render("ratio")
            ));
        }
        out.push('\n');
    }
    out.push_str(
        "> 功能、稳定性、架构、依赖与兼容性的对比见第三、六节；性能对比只使用同机、同模型、\
         同 case set 的数据，否则标记 `NOT_COMPARABLE`。\n\n",
    );
}

fn write_architecture(out: &mut String, context: &Context, transaction: &Transaction) {
    out.push_str("## 六、架构变化\n\n");
    let changes = changes(context, transaction);
    let crates = changes.at("changed_crates").as_arr().unwrap_or(&[]);
    out.push_str(&format!(
        "- 触及的 crate/顶层目录（{} 个）：{}\n",
        crates.len(),
        if crates.is_empty() {
            "N/A".to_string()
        } else {
            crates
                .iter()
                .filter_map(Json::as_str)
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    let owners = changes.at("owners_touched").as_arr().unwrap_or(&[]);
    out.push_str(&format!(
        "- 触及的责任边界：{}\n",
        if owners.is_empty() {
            "无协议 / 持久化 / 权限 / 依赖 / CI 变化".to_string()
        } else {
            owners
                .iter()
                .filter_map(Json::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    let added = git(
        context.host,
        &context.repo,
        &[
            "diff",
            "--name-status",
            &format!(
                "{}..{}",
                transaction.previous_release.clone().unwrap_or_default(),
                transaction.candidate_sha
            ),
        ],
    )
    .unwrap_or_default();
    let mut new_files = 0;
    let mut removed_files = 0;
    for line in added.lines() {
        if line.starts_with("A\t") {
            new_files += 1;
        }
        if line.starts_with("D\t") {
            removed_files += 1;
        }
    }
    out.push_str(&format!(
        "- 新增文件 {new_files} 个，删除文件 {removed_files} 个\n"
    ));
    out.push_str("- 风险：以 dogfood 与 RC 的实际结果为准；本节只陈述变更范围，不代替验证。\n\n");
}

fn write_ci(out: &mut String, transaction: &Transaction) {
    out.push_str("## 七、CI/CD\n\n");
    let remote = &transaction.remote;
    out.push_str("| 项 | 值 |\n| --- | --- |\n");
    out.push_str(&format!(
        "| 推送的 SHA | {} |\n",
        remote
            .pushed_sha
            .clone()
            .unwrap_or_else(|| "N/A".to_string())
    ));
    out.push_str(&format!(
        "| 必需 CI 结论 | {} |\n",
        remote
            .ci_conclusion
            .clone()
            .unwrap_or_else(|| "N/A".to_string())
    ));
    out.push_str(&format!(
        "| Workflow Run ID | {} |\n",
        remote
            .workflow_run_id
            .clone()
            .unwrap_or_else(|| "N/A".to_string())
    ));
    out.push_str(&format!(
        "| Workflow 状态 | {}/{} |\n",
        remote
            .workflow_run_status
            .clone()
            .unwrap_or_else(|| "N/A".to_string()),
        remote
            .workflow_run_conclusion
            .clone()
            .unwrap_or_else(|| "N/A".to_string())
    ));
    out.push_str(&format!(
        "| Tag | {} → {} |\n",
        remote.tag.clone().unwrap_or_else(|| "N/A".to_string()),
        remote.tag_sha.clone().unwrap_or_else(|| "N/A".to_string())
    ));
    out.push_str(&format!(
        "| Release | {} |\n",
        remote
            .release_url
            .clone()
            .unwrap_or_else(|| "N/A".to_string())
    ));
    out.push_str(&format!(
        "| 发布资产（{}）| {} |\n",
        remote.assets.len(),
        if remote.assets.is_empty() {
            "N/A".to_string()
        } else {
            remote.assets.join(", ")
        }
    ));
    out.push_str(&format!(
        "| 已校验 SHA256 的资产（{}）| {} |\n\n",
        remote.verified_assets.len(),
        if remote.verified_assets.is_empty() {
            "N/A".to_string()
        } else {
            remote.verified_assets.join(", ")
        }
    ));
}

fn write_recovery(out: &mut String, transaction: &Transaction) {
    out.push_str("## 八、失败恢复记录\n\n");
    if transaction.attempts.is_empty() {
        out.push_str("- 本次发布没有失败尝试。\n\n");
        return;
    }
    out.push_str("| 时间 | Stage | 状态 | 候选 SHA | 原因 |\n| --- | --- | --- | --- | --- |\n");
    for attempt in &transaction.attempts {
        out.push_str(&format!(
            "| {} | {} {} | {} | `{}` | {} |\n",
            crate::clock::iso8601(attempt.at),
            attempt.stage,
            state::stage_name(attempt.stage),
            attempt.status.label(),
            state::short_sha(&attempt.candidate_sha),
            attempt.reason.clone().unwrap_or_else(|| "-".to_string())
        ));
    }
    out.push('\n');
    if !transaction.changed_paths.is_empty() {
        out.push_str(&format!(
            "候选变更后重新执行的路径影响分析（{} 个文件）：\n\n",
            transaction.changed_paths.len()
        ));
        for path in transaction.changed_paths.iter().take(50) {
            out.push_str(&format!("- `{path}`\n"));
        }
        if transaction.changed_paths.len() > 50 {
            out.push_str(&format!(
                "- … 另外 {} 个文件，完整列表见 release.json\n",
                transaction.changed_paths.len() - 50
            ));
        }
        out.push('\n');
    }
    out.push_str("复用内容与重新执行内容以第二节的阶段状态为准：`REUSED` 表示输入指纹未变，`PASS` 表示本次现场执行。\n\n");
}

fn write_conclusion(out: &mut String, transaction: &Transaction, verdict: &str, note: &str) {
    out.push_str("## 九、正式结论\n\n");
    out.push_str(&format!("**{verdict}**\n\n"));
    out.push_str(&format!("{note}\n\n"));
    let published = transaction.remote.release_url.clone();
    match (verdict, published) {
        ("RELEASED", Some(url)) => out.push_str(&format!(
            "正式发布地址：{url}\n\n仅有本地证据、或仅有 Workflow dispatch 成功，都不构成发布完成；\
             本结论以 tag、Release、公开资产与 SHA256 的实际校验结果为准。\n"
        )),
        ("RELEASED", None) => {
            out.push_str("结论为 RELEASED 但未记录 Release URL，这本身是数据缺口，请检查第七节。\n")
        }
        _ => out.push_str(
            "未发布。版本号未被消耗：已经发布的版本不可变，本版本仍可在修复后通过 \
             `./dev publish --resume` 继续，不会自动跳到下一个版本。\n",
        ),
    }
}

/// The single verdict the report leads with. It is a summary of recorded facts,
/// not a new decision: `RELEASED` requires the remote to have accepted the
/// release, and `UNVERIFIED` is what a pass without artifact verification gets.
pub fn conclusion(transaction: &Transaction) -> (String, String) {
    if transaction.status == Status::Pass {
        let published = transaction.remote.release_state.as_deref() == Some("published");
        if published && !transaction.remote.verified_assets.is_empty() {
            return (
                "RELEASED".to_string(),
                "四个阶段全部 PASS，远端 tag、Release 与公开资产的 SHA256 均已校验。".to_string(),
            );
        }
        if published {
            return (
                "UNVERIFIED".to_string(),
                "远端 Release 已发布，但公开制品未被重新校验（缺少 SHA256 或使用了 \
                 --no-artifact-verify）；发布本身不可撤销，因此不标记为 RELEASED。"
                    .to_string(),
            );
        }
        return (
            "UNVERIFIED".to_string(),
            "本地阶段全部 PASS，但没有远端发布事实；本地通过不等于发布完成。".to_string(),
        );
    }
    if transaction.status == Status::Fail {
        return (
            "FAILED".to_string(),
            format!(
                "失败位置：{}。修复后请使用 `./dev publish --resume`，版本号保持 {}。",
                transaction
                    .failure_reason
                    .clone()
                    .unwrap_or_else(|| "UNKNOWN".to_string()),
                transaction.release_id
            ),
        );
    }
    if transaction.status == Status::Blocked {
        return (
            "BLOCKED".to_string(),
            "存在需要外部动作才能继续的阻塞项；具体原因见第三节与第八节。".to_string(),
        );
    }
    (
        "PENDING".to_string(),
        format!("发布仍在进行中：当前 stage {}。", transaction.stage),
    )
}
