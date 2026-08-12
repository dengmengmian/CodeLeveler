//! The `eval` subcommand: run/compare evaluation cases against models and
//! write durable baseline artifacts.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_agent::StopReason;
use leveler_app::Application;
use leveler_engine::ExecutionOverrides;
use leveler_execution::PermissionProfile;
use leveler_model::{ModelRef, ModelRuntime};
use leveler_project::Layout;

use crate::cli::EvalCommand;
use crate::common::resolve_model;
use crate::output::Line;

pub(crate) async fn cmd_eval(
    layout: Layout,
    command: EvalCommand,
) -> anyhow::Result<std::process::ExitCode> {
    let config_dir = layout.config_dir.clone();
    match command {
        EvalCommand::Run {
            model,
            cases,
            direct,
            no_verify_gate,
            repetitions,
            json_out,
        } => {
            let app = Application::assemble(layout)?;
            let model_ref = resolve_model(&app, model)?;
            let cases_dir = cases.clone();
            let cases = leveler_eval::EvaluationCase::load_dir(&cases)
                .map_err(|e| anyhow::anyhow!("loading cases: {e}"))?;
            if cases.is_empty() {
                anyhow::bail!("no eval cases found");
            }
            let mode = if no_verify_gate {
                "direct-no-verify-gate"
            } else {
                "direct"
            };
            println!("  mode: {mode}");
            let checkpoint = json_out.as_deref().map(checkpoint_path);
            let report = run_eval(
                &config_dir,
                &model_ref,
                &cases,
                direct,
                no_verify_gate,
                repetitions,
                None,
                checkpoint.as_deref(),
            )
            .await;
            print_eval_report(&report);
            if let Some(path) = json_out {
                let doc = leveler_eval::BaselineDocument::from_run(
                    baseline_meta(
                        &cases_dir,
                        mode,
                        repetitions,
                        std::slice::from_ref(&model_ref),
                        &cases,
                    ),
                    report.clone(),
                );
                write_baseline(&path, &doc)?;
            }
            Ok(if report.passed_count() == report.total() {
                std::process::ExitCode::SUCCESS
            } else {
                std::process::ExitCode::FAILURE
            })
        }
        EvalCommand::Compare {
            model_a,
            model_b,
            cases,
            repetitions,
            json_out,
        } => {
            let app = Application::assemble(layout)?;
            let a = resolve_model(&app, Some(model_a))?;
            let b = resolve_model(&app, Some(model_b))?;
            let cases_dir = cases.clone();
            let cases = leveler_eval::EvaluationCase::load_dir(&cases)
                .map_err(|e| anyhow::anyhow!("loading cases: {e}"))?;
            let checkpoint = json_out.as_deref().map(checkpoint_path);
            let cp = checkpoint.as_deref();
            let report_a =
                run_eval(&config_dir, &a, &cases, false, false, repetitions, None, cp).await;
            let report_b =
                run_eval(&config_dir, &b, &cases, false, false, repetitions, None, cp).await;
            print_eval_report(&report_a);
            print_eval_report(&report_b);
            let cmp = leveler_eval::Comparison::of(&report_a, &report_b);
            println!("{}", Line::heading("Model gap"));
            println!("  {} : {:.0}%", cmp.a, cmp.a_rate * 100.0);
            println!("  {} : {:.0}%", cmp.b, cmp.b_rate * 100.0);
            println!("  gap : {:.0} percentage points", cmp.model_gap * 100.0);

            println!("\n{}", Line::heading("Effort gap"));
            if cmp.paired_cases == 0 {
                println!("  no case passed under both models — nothing comparable");
            } else {
                println!("  over {} case(s) both models passed:", cmp.paired_cases);
                println!("  {} : {:.1} rounds", cmp.a, cmp.a_avg_rounds);
                println!("  {} : {:.1} rounds", cmp.b, cmp.b_avg_rounds);
                println!("  gap : {:+.1} rounds", cmp.effort_gap);
                if cmp.model_gap == 0.0 {
                    println!(
                        "  (completion is saturated at {:.0}% — the case set no longer \
                         separates these models on pass/fail; only effort does)",
                        cmp.a_rate * 100.0
                    );
                }
            }
            if let Some(path) = json_out {
                let doc = leveler_eval::BaselineDocument::from_compare(
                    baseline_meta(
                        &cases_dir,
                        "direct",
                        repetitions,
                        &[a.clone(), b.clone()],
                        &cases,
                    ),
                    report_a,
                    report_b,
                );
                write_baseline(&path, &doc)?;
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        EvalCommand::Ablate {
            knob,
            model,
            cases,
            direct,
            repetitions,
            json_out,
        } => {
            let app = Application::assemble(layout)?;
            let model_ref = resolve_model(&app, model)?;
            let cases_dir = cases.clone();
            let cases = leveler_eval::EvaluationCase::load_dir(&cases)
                .map_err(|e| anyhow::anyhow!("loading cases: {e}"))?;
            if cases.is_empty() {
                anyhow::bail!("no eval cases found");
            }
            let (ablated_overrides, before, after) = ablation_overrides(&knob)?;
            // Eval always uses the direct tool loop; --direct is a no-op compat flag.
            let mode = "direct";
            println!("  mode: {mode}");
            println!(
                "  ablation: {knob} = {before} (control) vs {after} (ablated), single variable"
            );

            let checkpoint = json_out.as_deref().map(checkpoint_path);
            let cp = checkpoint.as_deref();
            let control = run_eval(
                &config_dir,
                &model_ref,
                &cases,
                direct,
                false,
                repetitions,
                None,
                cp,
            )
            .await;
            let ablated = run_eval(
                &config_dir,
                &model_ref,
                &cases,
                direct,
                false,
                repetitions,
                Some(&ablated_overrides),
                cp,
            )
            .await;
            print_eval_report(&control);
            print_eval_report(&ablated);

            let verdict = leveler_eval::Ablation::of(&knob, &control, &ablated);
            println!("\n{}", Line::heading(&format!("Ablation: {knob}")));
            println!(
                "  control ({knob}={before}) : {:.0}%",
                verdict.control_rate * 100.0
            );
            println!(
                "  ablated ({knob}={after}) : {:.0}%",
                verdict.ablated_rate * 100.0
            );
            println!(
                "  rate delta : {:+.1}pp{}",
                verdict.rate_delta * 100.0,
                if verdict.rate_delta < 0.0 {
                    "  (the knob is saving cases)"
                } else if verdict.rate_delta > 0.0 {
                    "  (the knob is costing cases)"
                } else {
                    ""
                }
            );
            let list = |cases: &[String]| {
                if cases.is_empty() {
                    "(none)".to_string()
                } else {
                    cases.join(", ")
                }
            };
            println!("  saved by knob : {}", list(&verdict.saved_by_knob));
            println!("  hurt by knob  : {}", list(&verdict.hurt_by_knob));
            if !verdict.discarded_cases.is_empty() {
                println!(
                    "  discarded (infrastructure died in one arm — not knob evidence): {}",
                    verdict.discarded_cases.join(", ")
                );
            }
            if verdict.paired_cases == 0 {
                println!("  rounds: no case passed under both arms — nothing comparable");
            } else {
                println!(
                    "  rounds over {} paired case(s): control {:.1}, ablated {:.1}, delta {:+.1}",
                    verdict.paired_cases,
                    verdict.control_avg_rounds,
                    verdict.ablated_avg_rounds,
                    verdict.rounds_delta
                );
            }

            if let Some(path) = json_out {
                let doc = leveler_eval::BaselineDocument::from_compare(
                    baseline_meta(
                        &cases_dir,
                        &format!("{mode}-ablate-{knob}"),
                        repetitions,
                        std::slice::from_ref(&model_ref),
                        &cases,
                    ),
                    control,
                    ablated,
                );
                write_baseline(&path, &doc)?;
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
        EvalCommand::Quick {
            model,
            repetitions,
            json_out,
        } => {
            let app = Application::assemble(layout)?;
            let model_ref = resolve_model(&app, model)?;
            run_tier(
                &config_dir,
                &model_ref,
                "quick",
                &["evals/smoke"],
                repetitions,
                json_out,
            )
            .await
        }
        EvalCommand::Daily {
            model,
            repetitions,
            json_out,
        } => {
            let app = Application::assemble(layout)?;
            let model_ref = resolve_model(&app, model)?;
            run_tier(
                &config_dir,
                &model_ref,
                "daily",
                // Synthetic recovery scenarios join the daily gate; the heavy
                // real-repo scenarios stay in `release`.
                &["evals/core", "evals/hard", "evals/scenarios/debugging"],
                repetitions,
                json_out,
            )
            .await
        }
        EvalCommand::Release {
            model,
            repetitions,
            json_out,
        } => {
            let app = Application::assemble(layout)?;
            let model_ref = resolve_model(&app, model)?;
            run_tier(
                &config_dir,
                &model_ref,
                "release",
                &["evals/smoke", "evals/core", "evals/hard", "evals/scenarios"],
                repetitions,
                json_out,
            )
            .await
        }
        EvalCommand::Trend { history, out } => run_trend(&history, out),
    }
}

/// Build the version-over-version trend from a directory of run baselines.
/// Reuses the existing `--json-out` artifacts — no new result path — so history
/// is just "keep pointing `--json-out` at `evals/history/<version>.json`".
fn run_trend(
    history: &std::path::Path,
    out: Option<std::path::PathBuf>,
) -> anyhow::Result<std::process::ExitCode> {
    let entries = std::fs::read_dir(history)
        .map_err(|e| anyhow::anyhow!("reading history dir {}: {e}", history.display()))?;
    let mut points = Vec::new();
    let mut skipped = 0usize;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match leveler_eval::BaselineDocument::load_json(&path) {
            Ok(doc) => match doc.trend_point() {
                Some(p) => points.push(p),
                // Compare artifacts have no single score — expected, not an error.
                None => skipped += 1,
            },
            Err(e) => {
                eprintln!(
                    "  {} skipping {}: {e}",
                    console::style("!").yellow(),
                    path.display()
                );
                skipped += 1;
            }
        }
    }
    if points.is_empty() {
        anyhow::bail!(
            "no run baselines with a quality score in {} \
             (write some with `leveler eval quick --json-out {}/<version>.json`)",
            history.display(),
            history.display()
        );
    }
    let report = leveler_eval::TrendReport::from_points(points);
    let markdown = report.render_markdown();
    match out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&path, &markdown)
                .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
            println!(
                "  {} {} ({} version(s), {} skipped)",
                console::style("trend").dim(),
                path.display(),
                report.points.len(),
                skipped
            );
        }
        None => print!("{markdown}"),
    }
    // A regression is a soft signal here (reporting tool, not a gate): surface it
    // but exit 0 so CI can decide the policy.
    for r in report.regressions() {
        println!(
            "  {} regression {} → {}: {} points",
            console::style("⚠").red().bold(),
            r.from_version,
            r.to_version,
            r.score_delta
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Shared driver for the three tiered gates (spec §2). A tier is just a fixed
/// set of case directories run against one model — no new runner,
/// no new result path. Cases from all dirs are concatenated; a missing dir is a
/// hard error so a mistyped tier can't silently shrink coverage.
async fn run_tier(
    config_dir: &std::path::Path,
    model_ref: &ModelRef,
    tier: &str,
    dirs: &[&str],
    repetitions: u32,
    json_out: Option<std::path::PathBuf>,
) -> anyhow::Result<std::process::ExitCode> {
    let mut cases = Vec::new();
    for dir in dirs {
        let loaded = leveler_eval::EvaluationCase::load_dir(std::path::Path::new(dir))
            .map_err(|e| anyhow::anyhow!("loading {dir}: {e}"))?;
        cases.extend(loaded);
    }
    if cases.is_empty() {
        anyhow::bail!(
            "tier `{tier}` found no cases in [{}] — run from the repo root; \
             for release, fetch real repos first (scripts/fetch_eval_repos.sh)",
            dirs.join(", ")
        );
    }
    println!(
        "  tier: {tier} ({} cases across {})",
        cases.len(),
        dirs.join(", ")
    );
    println!("  mode: direct");
    let checkpoint = json_out.as_deref().map(checkpoint_path);
    let report = run_eval(
        config_dir,
        model_ref,
        &cases,
        false,
        false,
        repetitions,
        None,
        checkpoint.as_deref(),
    )
    .await;
    print_eval_report(&report);
    if let Some(path) = json_out {
        let doc = leveler_eval::BaselineDocument::from_run(
            baseline_meta(
                std::path::Path::new(&dirs.join(",")),
                &format!("tier-{tier}"),
                repetitions,
                std::slice::from_ref(model_ref),
                &cases,
            ),
            report.clone(),
        );
        write_baseline(&path, &doc)?;
    }
    Ok(if report.passed_count() == report.total() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

/// Flip one boolean policy knob in place; returns `(before, after)` for the
/// run banner. The knob names mirror the `configs/policies/*.yaml` fields.
/// Build the ablated arm's overrides for one knob: exactly one resolver input
/// flipped away from its default. Legacy `require_*` names stay as aliases so
/// existing scripts keep working.
///
/// Direction: control = production default, ablated = the flip that measures
/// "what is this knob worth". Rails that default ON ablate OFF; the plan gate
/// defaults ON in the resolver, so its ablated arm is `false`.
fn ablation_overrides(knob: &str) -> anyhow::Result<(ExecutionOverrides, bool, bool)> {
    let mut o = ExecutionOverrides::default();
    let (before, after) = match knob {
        "explicit_plan" | "require_explicit_plan" => {
            // Production resolve defaults to true; flip it off to measure value.
            o.explicit_plan = Some(false);
            (true, false)
        }
        "completion_evidence" | "require_completion_evidence" => {
            o.completion_evidence = Some(false);
            (true, false)
        }
        // Tools-layer read guard and agent-level progress heuristics share this
        // seam (factory wires both from the resolved flag).
        "repeated_read_guard" | "progress_guards" => {
            o.repeated_read_guard = Some(false);
            (true, false)
        }
        // C5-S3: control = static budget (production default OFF); ablated =
        // the adaptive candidate ON. Note the direction: this knob measures a
        // CANDIDATE, so "ablated" is the arm with the mechanism enabled.
        "adaptive_context" => {
            o.adaptive_context = Some(true);
            (false, true)
        }
        _ => anyhow::bail!(
            "unknown knob `{knob}` — expected one of: explicit_plan, \
             completion_evidence, repeated_read_guard, progress_guards, \
             adaptive_context"
        ),
    };
    Ok((o, before, after))
}

/// The per-case checkpoint file that shadows a baseline: `x.json` → `x.partial.jsonl`.
fn checkpoint_path(json_out: &std::path::Path) -> std::path::PathBuf {
    json_out.with_extension("partial.jsonl")
}

/// Build baseline metadata for a durable eval artifact.
fn baseline_meta(
    cases_dir: &std::path::Path,
    mode: &str,
    repetitions: u32,
    models: &[ModelRef],
    cases: &[leveler_eval::EvaluationCase],
) -> leveler_eval::BaselineMeta {
    let repository_cases = cases.iter().filter(|case| case.repo.is_some()).count();
    leveler_eval::BaselineMeta {
        created_at: utc_now_rfc3339(),
        git_sha: git_head_sha(),
        cases_dir: cases_dir.display().to_string(),
        mode: mode.to_string(),
        repetitions,
        model_refs: models.iter().map(ToString::to_string).collect(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        context: leveler_eval::BaselineContext {
            case_count: cases.len(),
            repository_cases,
            synthetic_cases: cases.len().saturating_sub(repository_cases),
        },
    }
}

fn write_baseline(
    path: &std::path::Path,
    doc: &leveler_eval::BaselineDocument,
) -> anyhow::Result<()> {
    doc.write_json(path)
        .map_err(|e| anyhow::anyhow!("writing --json-out {}: {e}", path.display()))?;
    println!("  {} {}", console::style("json-out").dim(), path.display());
    Ok(())
}

fn utc_now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Best-effort `git rev-parse HEAD` from the process CWD (empty tree → None).
fn git_head_sha() -> Option<String> {
    let out = leveler_core::git_stdout(std::path::Path::new("."), &["rev-parse", "HEAD"])?;
    let s = out.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn scrub_command_env(command: &mut std::process::Command) {
    command.env_clear();
    command.envs(leveler_core::scrubbed_environment());
}

/// Run every case against one model in an isolated temp repo.
///
/// `checkpoint` (when set) receives one JSON line per finished case. A Hard-set
/// run is hours long; without it, an interrupt anywhere before the final write
/// loses every completed case. The file is append-only and self-describing, so
/// a killed run's cases can still be recovered and compared.
/// Serial number for one `run_eval_case` invocation in this process.
///
/// Workspace identity has to be per *execution*, not per case: `compare` runs
/// two models and `ablate` runs control and treatment, each calling `run_eval`
/// again in the same process, so `case + pid + repetition` still collides.
static EXECUTION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_execution_seq() -> u64 {
    EXECUTION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// Where one case execution does its work.
///
/// Every execution gets its own directory. The path used to be
/// `<case>-<pid>`, and `run_eval_case` wipes its directory on entry, so three
/// repetitions of a case in one process each deleted the previous one's tree —
/// including under `LEVELER_EVAL_KEEP_WORKSPACE=1`, which only governs cleanup
/// at the end. Nothing noticed while a run was scored by its exit status, and
/// it becomes fatal the moment anything needs the tree a *particular* run left
/// behind, which per-obligation implementation coverage does.
///
/// The case id and pid stay in the name because they are what a human looks
/// for; `exec` and `r` are what make it unique.
fn case_workspace(case_id: &str, pid: u32, execution: u64, repetition: u32) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "leveler-eval-{case_id}-{pid}-exec{execution}-r{repetition}"
    ))
}

/// Put the answers out of reach of every command the agent can run.
///
/// A benchmark keeps its answer key on the same machine as the thing it is
/// grading: case definitions with their hidden relevant/impact paths, the
/// hidden acceptance scripts, the pristine fixture a workspace was cloned
/// from, and this run's own event log. `read_file` already refuses paths
/// outside the workspace — but an eval run was observed reaching all of it
/// with `cat`, `git -C`, `python3` and `sqlite3` after finding the host path,
/// which is what a tool-layer guard cannot prevent.
///
/// So the boundary moves below the command, into the sandbox profile, where it
/// is enforced on the resolved path and does not care which tool asked. The
/// workspace, the toolchain and the scratch space are untouched; agents keep
/// full `git log`/`show`/`blame`/`diff` and normal builds inside the clone.
///
/// Sealed once, before any case runs; every command built afterwards carries it.
fn seal_eval_answer_keys() {
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    // The repository this harness was launched from: cases, fixtures, hidden
    // acceptance, and the source of the agent grading itself.
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    // Session/event storage — the run's own transcript, which carries the task,
    // the tool history, and enough to reconstruct the setup. Sealed by the
    // subdirectory, not by `~/.leveler` as a whole: the toolchain cache lives
    // in that tree too, and sealing it stops `go build` from working at all.
    // Via LevelerHome so LEVELER_HOME is honoured and the canonical layout is
    // sealed (the toolchain cache under the same tree is deliberately NOT sealed).
    let home = leveler_core::LevelerHome::resolve(leveler_core::environment());
    roots.push(home.projects_dir());
    roots.push(home.config_file());
    leveler_execution::seal_read_denials(roots);
}

async fn run_eval(
    config_dir: &std::path::Path,
    model: &ModelRef,
    cases: &[leveler_eval::EvaluationCase],
    direct: bool,
    no_verify_gate: bool,
    repetitions: u32,
    overrides: Option<&ExecutionOverrides>,
    checkpoint: Option<&std::path::Path>,
) -> leveler_eval::EvalReport {
    seal_eval_answer_keys();
    let mut results = Vec::new();
    for case in cases {
        for repetition in 1..=repetitions {
            println!(
                "{} {} ({}, run {}/{})",
                console::style("▶ eval").magenta().bold(),
                case.name,
                case.id,
                repetition,
                repetitions
            );
            let result = run_eval_case(
                config_dir,
                model,
                case,
                direct,
                no_verify_gate,
                repetition,
                overrides,
            )
            .await;
            let mark = if result.passed() {
                console::style("✓").green()
            } else {
                console::style("✗").red()
            };
            println!("  {mark} {}#{} — {}", case.id, repetition, result.note);
            if let Some(path) = checkpoint {
                append_checkpoint(path, model, overrides.is_some(), &result);
            }
            results.push(result);
        }
    }
    leveler_eval::EvalReport {
        model: model.to_string(),
        cases: results,
    }
}

/// Append one finished case to the run's checkpoint file. Best-effort: a
/// checkpoint IO failure must never abort an eval that is otherwise fine, so it
/// warns and continues rather than losing the run it exists to protect.
fn append_checkpoint(
    path: &std::path::Path,
    model: &ModelRef,
    ablated: bool,
    result: &leveler_eval::CaseResult,
) {
    use std::io::Write;
    let line = serde_json::json!({
        "model": model.to_string(),
        "arm": if ablated { "ablated" } else { "control" },
        "case": result,
    });
    let write = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = write {
        eprintln!(
            "warning: could not write checkpoint {}: {e}",
            path.display()
        );
    }
}

/// Drive the engine's direct path with the verification gate REMOVED — the one
/// ablated variable. Everything else (prompt, completion audit, loop guard,
/// apply_patch tolerance, compaction, round budget) is identical to the normal
/// direct run, so a difference in `expect_passed` is attributable to the gate
/// and its repair turn alone.
///
/// `completed` here means "the model said it was done" — with no gate there is
/// nothing to verify against, so the engine can only report `CompletedUnverified`.
/// The case still passes or fails on the independent `expect` command.
async fn run_bare_case(
    app: &Application,
    model: &ModelRef,
    case: &leveler_eval::EvaluationCase,
    collector: &mut crate::eval_signals::SignalCollector,
) -> (
    Option<leveler_core::SessionId>,
    bool,
    u32,
    String,
    leveler_eval::TerminationClass,
    Option<leveler_eval::FailureCategory>,
) {
    let engine = match app
        .engine_for(
            model,
            PermissionProfile::Assisted,
            false,
            std::sync::Arc::new(leveler_execution::EvalApprove),
            Arc::new(leveler_agent::AutoClarify),
        )
        .await
    {
        Ok(engine) => engine,
        Err(e) => {
            let termination = termination_from_app_error(&e);
            let cause = infrastructure_cause_from_app_error(&e);
            return (None, false, 0, format!("engine: {e}"), termination, cause);
        }
    };
    let spec = leveler_engine::TaskSpec {
        runtime: leveler_engine::RuntimeTaskSpec {
            goal: case.task.clone(),
            kind: leveler_engine::ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::bounded(case.max_rounds),
            limits: leveler_agent::StepLimits::default(),
        },
        coding: leveler_engine::CodingTaskSpec {
            repository: app.layout.repo_root.clone(),
            mode: PermissionProfile::Assisted,
            sandbox: false,
            // THE ablated variable: an empty plan means there is nothing to verify.
            verification: leveler_verifier::VerificationPlan::default(),
            base_commit: None,
        },
    };
    let session_id = match engine.create_task(&spec).await {
        Ok(id) => id,
        Err(e) => {
            let termination = termination_from_engine_error(&e);
            let cause = infrastructure_cause_from_engine_error(&e);
            return (None, false, 0, format!("session: {e}"), termination, cause);
        }
    };
    let result = engine
        .run(
            &session_id,
            &spec,
            &mut |event| collector.observe_engine(event),
            CancellationToken::new(),
        )
        .await;
    match result {
        Ok(report) => {
            let termination = termination_from_report(report.outcome, report.stop_reason);
            (
                Some(session_id),
                report.outcome.is_success(),
                report.rounds,
                format!("{:?}", report.outcome),
                termination,
                None,
            )
        }
        Err(e) => {
            let termination = termination_from_engine_error(&e);
            let cause = infrastructure_cause_from_engine_error(&e);
            (
                Some(session_id),
                false,
                0,
                format!("error: {e}"),
                termination,
                cause,
            )
        }
    }
}

/// Count engine repair turns and whether verification passed after the last
/// one, from the persisted event log. `repair_started` / `verification_finished`
/// are the engine's own canonical rows — a repair is only reported when the
/// engine actually started one.
async fn repair_metrics_from_events(
    db: &leveler_storage::Database,
    session_id: &leveler_core::SessionId,
) -> (u32, Option<bool>) {
    let events = match leveler_storage::EventRepository::new(db)
        .load(session_id)
        .await
    {
        Ok(events) => events,
        Err(_) => return (0, None),
    };
    let mut repairs = 0u32;
    let mut last_repair_seq: Option<i64> = None;
    for event in &events {
        if event.event_type == "repair_started" {
            repairs += 1;
            last_repair_seq = Some(event.sequence);
        }
    }
    let Some(repair_seq) = last_repair_seq else {
        return (0, None);
    };
    // The first verification verdict AFTER the last repair is its outcome.
    let post_repair_verdict = events
        .iter()
        .filter(|e| e.sequence > repair_seq && e.event_type == "verification_finished")
        .find_map(|e| {
            serde_json::from_str::<serde_json::Value>(&e.payload)
                .ok()?
                .get("payload")?
                .get("passed")?
                .as_bool()
        });
    (repairs, post_repair_verdict)
}

fn termination_from_stop_reason(reason: StopReason) -> leveler_eval::TerminationClass {
    match reason {
        StopReason::Completed
        | StopReason::Answered
        | StopReason::CompletedUnverified
        | StopReason::CloseoutForced => leveler_eval::TerminationClass::Completed,
        StopReason::BudgetExhausted => leveler_eval::TerminationClass::BudgetLimited,
        StopReason::TurnLimitReached => leveler_eval::TerminationClass::BudgetLimited,
        StopReason::Blocked => leveler_eval::TerminationClass::Blocked,
        StopReason::Incomplete | StopReason::Stalled => leveler_eval::TerminationClass::Incomplete,
    }
}

fn termination_from_report(
    outcome: leveler_engine::TaskOutcome,
    stop_reason: StopReason,
) -> leveler_eval::TerminationClass {
    let termination = termination_from_stop_reason(stop_reason);
    if outcome == leveler_engine::TaskOutcome::Failed
        && termination == leveler_eval::TerminationClass::Completed
    {
        leveler_eval::TerminationClass::Failed
    } else {
        termination
    }
}

fn termination_from_model_error(
    error: &leveler_model::ModelError,
) -> leveler_eval::TerminationClass {
    match error.kind {
        leveler_model::ModelErrorKind::RateLimit => leveler_eval::TerminationClass::UsageLimited,
        leveler_model::ModelErrorKind::Cancelled => leveler_eval::TerminationClass::Incomplete,
        _ => leveler_eval::TerminationClass::InfrastructureFailed,
    }
}

/// First-cause category for a run killed by a model-layer error, from the
/// STRUCTURED kind — never from the vendor's message text. `Some` exactly for
/// the kinds `termination_from_model_error` books as `InfrastructureFailed`,
/// so the execution boundary and the attribution never disagree.
fn infrastructure_cause_from_model_error(
    error: &leveler_model::ModelError,
) -> Option<leveler_eval::FailureCategory> {
    use leveler_model::ModelErrorKind as K;
    match error.kind {
        // Not infrastructure boundaries (UsageLimited / Incomplete).
        K::RateLimit | K::Cancelled => None,
        // Wire/protocol contract violations: the provider rejected the request
        // as invalid, or sent a body we could not decode.
        K::InvalidRequest | K::Decode => Some(leveler_eval::FailureCategory::ProviderProtocol),
        // Reachability, credentials, and provider-side behavior: the
        // environment around the run, not the protocol contract.
        K::Auth
        | K::ProviderUnavailable
        | K::Transport
        | K::Timeout
        | K::StreamInterrupted
        | K::Truncated
        | K::ContentFiltered
        | K::Other => Some(leveler_eval::FailureCategory::Environment),
    }
}

/// First-cause category for an app-layer death. Non-model failures are the
/// harness/framework's own — never `ProviderProtocol`.
fn infrastructure_cause_from_app_error(
    error: &leveler_app::AppError,
) -> Option<leveler_eval::FailureCategory> {
    match error {
        leveler_app::AppError::Model(error)
        | leveler_app::AppError::Agent(leveler_agent::AgentError::Model(error)) => {
            infrastructure_cause_from_model_error(error)
        }
        leveler_app::AppError::Agent(leveler_agent::AgentError::Cancelled) => None,
        // Setup around the run: config, workspace, filesystem.
        leveler_app::AppError::Config(_)
        | leveler_app::AppError::GlobalConfig(_)
        | leveler_app::AppError::Registry(_)
        | leveler_app::AppError::Workspace(_)
        | leveler_app::AppError::NotFound(_)
        | leveler_app::AppError::Io { .. }
        | leveler_app::AppError::RuntimeIdentity(_) => {
            Some(leveler_eval::FailureCategory::Environment)
        }
        // Storage / engine internals / everything else: framework failure.
        _ => Some(leveler_eval::FailureCategory::Runtime),
    }
}

/// First-cause category for an engine-layer death — same policy as app errors.
fn infrastructure_cause_from_engine_error(
    error: &leveler_engine::EngineError,
) -> Option<leveler_eval::FailureCategory> {
    match error {
        leveler_engine::EngineError::Agent(leveler_agent::AgentError::Model(error)) => {
            infrastructure_cause_from_model_error(error)
        }
        leveler_engine::EngineError::Agent(leveler_agent::AgentError::Cancelled) => None,
        _ => Some(leveler_eval::FailureCategory::Runtime),
    }
}

fn termination_from_app_error(error: &leveler_app::AppError) -> leveler_eval::TerminationClass {
    match error {
        leveler_app::AppError::Model(error)
        | leveler_app::AppError::Agent(leveler_agent::AgentError::Model(error)) => {
            termination_from_model_error(error)
        }
        leveler_app::AppError::Agent(leveler_agent::AgentError::Cancelled) => {
            leveler_eval::TerminationClass::Incomplete
        }
        _ => leveler_eval::TerminationClass::InfrastructureFailed,
    }
}

fn termination_from_engine_error(
    error: &leveler_engine::EngineError,
) -> leveler_eval::TerminationClass {
    match error {
        leveler_engine::EngineError::Agent(leveler_agent::AgentError::Model(error)) => {
            termination_from_model_error(error)
        }
        leveler_engine::EngineError::Agent(leveler_agent::AgentError::Cancelled) => {
            leveler_eval::TerminationClass::Incomplete
        }
        _ => leveler_eval::TerminationClass::InfrastructureFailed,
    }
}

async fn run_eval_case(
    config_dir: &std::path::Path,
    model: &ModelRef,
    case: &leveler_eval::EvaluationCase,
    direct: bool,
    no_verify_gate: bool,
    repetition: u32,
    overrides: Option<&ExecutionOverrides>,
) -> leveler_eval::CaseResult {
    use std::process::Command as Proc;

    let started = std::time::Instant::now();
    let fail = |note: String, failure_category| leveler_eval::CaseResult {
        id: case.id.clone(),
        repetition,
        completed: false,
        termination: Some(leveler_eval::TerminationClass::InfrastructureFailed),
        expect_passed: false,
        rounds: 0,
        latency_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd_micros: None,
        failure_category: Some(failure_category),
        failure_source: Some(leveler_eval::FailureSource::Auto),
        note,
        verification_evidence: None,
        tool_calls: 0,
        loop_guard_trips: 0,
        verification_ran: false,
        is_recovery: case.recovery,
        ttff_ms: None,
        silent_duration_ms: None,
        edit_attempts: 0,
        edit_failures: 0,
        read_calls: 0,
        search_calls: 0,
        apply_patch_calls: 0,
        replace_calls: 0,
        first_edit_round: None,
        unique_files_read: 0,
        repeated_file_reads: 0,
        unique_search_queries: 0,
        repeated_search_queries: 0,
        first_relevant_file_round: None,
        first_plan_round: None,
        relevant_paths_touched: 0,
        relevant_paths_before_edit: 0,
        impact_paths_touched: 0,
        impact_paths_before_edit: 0,
        distractor_paths_read: 0,
        forbidden_paths_edited: 0,
        broad_reads: 0,
        narrow_reads: 0,
        verification_driven_impact_discovery: false,
        missed_impact_paths: Vec::new(),
        repair_attempts: 0,
        repair_success: None,
    };

    // Materialize the workspace. Two modes:
    //  - synthetic: an empty repo seeded entirely from `case.files`.
    //  - repo:      clone a real git repo, then overlay `case.files` on top so
    //               the agent must locate the relevant code in a full codebase.
    let dir = case_workspace(
        &case.id,
        std::process::id(),
        next_execution_seq(),
        repetition,
    );
    let _ = std::fs::remove_dir_all(&dir);

    let overlay_files = |dir: &std::path::Path| -> Result<(), String> {
        for (rel, content) in &case.files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&path, content).map_err(|_| format!("could not write {rel}"))?;
        }
        Ok(())
    };
    let git = |args: &[&str]| {
        let mut command = Proc::new("git");
        command.args(args).current_dir(&dir);
        scrub_command_env(&mut command);
        command.output()
    };

    if let Some(repo) = &case.repo {
        // Fast local clone of the real repo (HEAD, or `base_ref` if given).
        let src = std::fs::canonicalize(repo).unwrap_or_else(|_| std::path::PathBuf::from(repo));
        let mut clone_command = Proc::new("git");
        clone_command
            .args(["clone", "--local", "--quiet"])
            .arg(&src)
            .arg(&dir);
        scrub_command_env(&mut clone_command);
        let clone = clone_command.output();
        match clone {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                return fail(
                    format!(
                        "git clone {} failed: {}",
                        src.display(),
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                    leveler_eval::FailureCategory::Environment,
                );
            }
            Err(e) => {
                return fail(
                    format!("git clone spawn: {e}"),
                    leveler_eval::FailureCategory::Environment,
                );
            }
        }
        if let Some(base) = &case.base_ref {
            let _ = git(&["checkout", "--quiet", base]);
        }
        // Drop the clone's origin. It points at the pristine fixture inside
        // this repository, and a run that reads it can diff its way to the
        // injected defect, or walk up to `evals/` and read the case file with
        // its hidden relevant/impact/distractor paths. Observed: an N6 run
        // followed exactly that path (`git remote -v` → `git -C <src> show` →
        // `ls .../evals/`). `read_file` refuses paths outside the workspace;
        // `shell_command` does not, so the breadcrumb has to go rather than
        // the guard being trusted to hold.
        let _ = git(&["remote", "remove", "origin"]);
        // Overlay the injected bug/failing test and commit it as the baseline.
        if let Err(e) = overlay_files(&dir) {
            return fail(e, leveler_eval::FailureCategory::Environment);
        }
        let _ = git(&["config", "user.email", "eval@leveler"]);
        let _ = git(&["config", "user.name", "leveler-eval"]);
        let _ = git(&["add", "-A"]);
        let _ = git(&["commit", "-qm", "eval setup"]);
    } else {
        if std::fs::create_dir_all(&dir).is_err() {
            return fail(
                "could not create workspace".into(),
                leveler_eval::FailureCategory::Environment,
            );
        }
        if let Err(e) = overlay_files(&dir) {
            return fail(e, leveler_eval::FailureCategory::Environment);
        }
        let _ = git(&["init", "-q"]);
        let _ = git(&["config", "user.email", "eval@leveler"]);
        let _ = git(&["config", "user.name", "leveler-eval"]);
        let _ = git(&["add", "-A"]);
        let _ = git(&["commit", "-qm", "eval baseline"]);
    }

    // Run the agent (direct tool loop) in the case workspace.
    let layout = Layout::resolve(dir.clone(), Some(config_dir.to_path_buf()));
    let app = match Application::assemble(layout) {
        Ok(a) => a,
        Err(e) => {
            return fail(
                format!("assemble: {e}"),
                leveler_eval::FailureCategory::Environment,
            );
        }
    };
    // Ablation arm: pin the flipped resolver input on every execution path.
    let app = match overrides {
        Some(overrides) => app.with_execution_overrides(overrides.clone()),
        None => app,
    };
    // Fold the event stream into trajectory signals for failure attribution
    // (L1 taskset doc §8); the overlay's paths proxy for "the relevant files".
    let mut collector = crate::eval_signals::SignalCollector::with_navigation_paths(
        case.files
            .keys()
            .cloned()
            .chain(case.relevant_paths.iter().cloned()),
        case.required_impact_paths.iter().cloned(),
        case.distractor_paths.iter().cloned(),
        case.forbidden_edit_paths.iter().cloned(),
    );
    // C1.5A ablation arm; `None` on every normal run (see eval_commitment).
    let commitment = crate::eval_commitment::CommitmentNudge::from_environment().map(Arc::new);
    // Eval runs the direct tool loop only (orchestrate dual path removed).
    // `infrastructure_cause` is the structured first-cause of an error-path
    // death (None when the run finished on its own), fed to attribution so
    // `InfrastructureFailed` alone never implies provider protocol.
    let (session_id, completed, rounds, mut note, termination, infrastructure_cause) =
        if no_verify_gate {
            // Ablation: the SAME direct loop with ONE variable removed — the
            // post-edit verification gate and the repair turn it drives.
            run_bare_case(&app, model, case, &mut collector).await
        } else {
            let _ = direct; // flag retained for CLI compatibility; always direct.
            match app.create_session(model, &case.task).await {
                Ok(session_id) => {
                    let outcome = app
                        .run_in_session_bounded(
                            &session_id,
                            model,
                            PermissionProfile::Assisted,
                            &case.task,
                            std::sync::Arc::new(leveler_execution::EvalApprove),
                            false,
                            &mut |e| {
                                collector.observe_agent(&e);
                                if let Some(nudge) = &commitment {
                                    nudge.observe(&e);
                                }
                            },
                            CancellationToken::new(),
                            case.max_rounds,
                            commitment
                                .clone()
                                .map(|n| n as std::sync::Arc<dyn leveler_agent::SteeringSource>),
                        )
                        .await;
                    match outcome {
                        Ok(o) => {
                            let termination = termination_from_stop_reason(o.stop_reason);
                            // `completed` = the run ended in the terminal
                            // outcome THIS CASE requires. Default: a verified
                            // completion (historical semantics). A declared
                            // `completed_unverified` case matches exactly that
                            // stop reason — and fails on a wrongful upgrade to
                            // a verified completion.
                            let completed = match case.expected_outcome {
                                leveler_eval::ExpectedOutcome::Completed => {
                                    o.stop_reason == StopReason::Completed
                                }
                                leveler_eval::ExpectedOutcome::CompletedUnverified => {
                                    o.stop_reason == StopReason::CompletedUnverified
                                }
                            };
                            (
                                Some(session_id),
                                completed,
                                o.rounds,
                                format!("{:?}", o.stop_reason),
                                termination,
                                None,
                            )
                        }
                        Err(e) => {
                            let termination = termination_from_app_error(&e);
                            let cause = infrastructure_cause_from_app_error(&e);
                            (
                                Some(session_id),
                                false,
                                0,
                                format!("error: {e}"),
                                termination,
                                cause,
                            )
                        }
                    }
                }
                Err(e) => {
                    let termination = termination_from_app_error(&e);
                    let cause = infrastructure_cause_from_app_error(&e);
                    (None, false, 0, format!("session: {e}"), termination, cause)
                }
            }
        };

    // Usage and the observed round count both come from the persisted model
    // requests. `rounds` is 0 on the error paths above (the outcome that carries
    // it never came back), so fall back to the request count — otherwise a failed
    // run reports zero effort, which is simply false.
    let (input_tokens, output_tokens, observed_rounds) = if let Some(session_id) = &session_id {
        match app.open_database().await {
            Ok(db) => leveler_storage::ModelRequestRepository::new(&db)
                .load_for_session(session_id)
                .await
                .map(|records| {
                    let requests = records.len() as u32;
                    let (input, output) =
                        records.into_iter().fold((0u64, 0u64), |total, record| {
                            (
                                total.0.saturating_add(record.input_tokens),
                                total.1.saturating_add(record.output_tokens),
                            )
                        });
                    (input, output, requests)
                })
                .unwrap_or_default(),
            Err(_) => (0, 0, 0),
        }
    } else {
        (0, 0, 0)
    };

    // Repair metrics come from the canonical persisted event log, not the
    // AgentEvent stream (the engine-to-agent mapping drops RepairStarted): a
    // repair only counts when the engine actually emitted the event.
    let (repair_attempts, repair_success) = if let Some(session_id) = &session_id {
        match app.open_database().await {
            Ok(db) => repair_metrics_from_events(&db, session_id).await,
            Err(_) => (0, None),
        }
    } else {
        (0, None)
    };
    let rounds = if rounds > 0 { rounds } else { observed_rounds };

    // Cost only when the model profile carries auditable pricing — never invented.
    let cost_usd_micros = match app.registry.profile(model).await {
        Ok(profile) => profile
            .pricing
            .map(|p| p.cost_usd_micros(input_tokens, output_tokens)),
        Err(_) => None,
    };

    // Evaluate the expectation independently (verification-driven, ).
    let (expect_passed, verification_exit_code) = {
        let mut command = Proc::new(&case.expect.program);
        command.args(&case.expect.args).current_dir(&dir);
        scrub_command_env(&mut command);
        let out = command.output();
        match out {
            Ok(o) => (o.status.success(), o.status.code()),
            Err(e) => {
                note = format!("expect spawn failed: {e}");
                (false, None)
            }
        }
    };

    // Diagnostics: `LEVELER_EVAL_KEEP_WORKSPACE=1` leaves the case workspace on
    // disk so a surprising verdict can be reproduced against the exact tree the
    // gates saw. Off by default — a normal run still cleans up.
    if std::env::var_os("LEVELER_EVAL_KEEP_WORKSPACE").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        // Deterministic mapping for offline analysis: a later scorer must never
        // have to guess which directory belongs to which run.
        println!(
            "  kept workspace: case={} repetition={} session={} path={}",
            case.id,
            repetition,
            session_id
                .as_ref()
                .map_or_else(|| "-".to_string(), |s| s.to_string()),
            dir.display()
        );
    }
    // First-cause attribution receives the structured budget marker rather
    // than parsing a debug-formatted outcome note.
    // Named before `finish` consumes the collector: a half-fix is only useful
    // as a diagnosis if it says which path was missed.
    let missed_impact_paths = collector.missed_impact_paths();
    let signals = collector.finish(termination == leveler_eval::TerminationClass::BudgetLimited);
    leveler_eval::CaseResult {
        id: case.id.clone(),
        repetition,
        completed,
        termination: Some(termination),
        expect_passed,
        rounds,
        latency_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        input_tokens,
        output_tokens,
        cost_usd_micros,
        failure_category: leveler_eval::attribute_failure(
            completed,
            expect_passed,
            infrastructure_cause,
            &signals,
        ),
        failure_source: (!(completed && expect_passed))
            .then_some(leveler_eval::FailureSource::Auto),
        note,
        verification_evidence: Some(leveler_eval::VerificationEvidence {
            program: case.expect.program.clone(),
            args: case.expect.args.clone(),
            passed: expect_passed,
            exit_code: verification_exit_code,
        }),
        tool_calls: signals.tool_calls,
        loop_guard_trips: signals.loop_guard_trips,
        verification_ran: signals.verification_ran,
        is_recovery: case.recovery,
        ttff_ms: signals.ttff_ms,
        silent_duration_ms: signals.max_silent_ms,
        edit_attempts: signals.edit_attempts,
        edit_failures: signals.edit_failures,
        read_calls: signals.read_calls,
        search_calls: signals.search_calls,
        apply_patch_calls: signals.apply_patch_calls,
        replace_calls: signals.replace_calls,
        first_edit_round: signals.first_edit_round,
        unique_files_read: signals.unique_files_read,
        repeated_file_reads: signals.repeated_file_reads,
        unique_search_queries: signals.unique_search_queries,
        repeated_search_queries: signals.repeated_search_queries,
        first_relevant_file_round: signals.first_relevant_file_round,
        first_plan_round: signals.first_plan_round,
        relevant_paths_touched: signals.relevant_paths_touched,
        relevant_paths_before_edit: signals.relevant_paths_before_edit,
        impact_paths_touched: signals.impact_paths_touched,
        impact_paths_before_edit: signals.impact_paths_before_edit,
        distractor_paths_read: signals.distractor_paths_read,
        forbidden_paths_edited: signals.forbidden_paths_edited,
        broad_reads: signals.broad_reads,
        narrow_reads: signals.narrow_reads,
        verification_driven_impact_discovery: signals.verification_driven_impact_discovery,
        missed_impact_paths,
        repair_attempts,
        repair_success,
    }
}

fn print_eval_report(report: &leveler_eval::EvalReport) {
    println!("\n{}", Line::heading(&format!("Eval: {}", report.model)));
    for c in &report.cases {
        let mark = if c.passed() {
            console::style("✓").green()
        } else {
            console::style("✗").red()
        };
        // Failed cases show their first-cause category; `*` marks a manual
        // override of the auto classifier.
        let category = match (&c.failure_category, &c.failure_source) {
            (Some(cat), source) => {
                let tag = serde_json::to_value(cat)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                let mark = if *source == Some(leveler_eval::FailureSource::Manual) {
                    "*"
                } else {
                    ""
                };
                format!(" [{tag}{mark}]")
            }
            (None, _) => String::new(),
        };
        let termination = c
            .termination
            .and_then(|value| serde_json::to_value(value).ok())
            .and_then(|value| value.as_str().map(str::to_string))
            .map(|value| format!(" termination={value}"))
            .unwrap_or_default();
        // Exploration/edit/repair shape, only when the run did any of it —
        // legacy runs and pure-answer cases keep the compact line.
        let mut shape = String::new();
        if c.read_calls + c.search_calls > 0 {
            shape.push_str(&format!(
                " reads={} searches={}",
                c.read_calls, c.search_calls
            ));
        }
        if c.edit_attempts > 0 {
            shape.push_str(&format!(
                " edits={}/{}err (patch={} replace={} first@r{})",
                c.edit_attempts,
                c.edit_failures,
                c.apply_patch_calls,
                c.replace_calls,
                c.first_edit_round.unwrap_or(0)
            ));
        }
        if c.repair_attempts > 0 {
            shape.push_str(&format!(
                " repair={}({})",
                c.repair_attempts,
                match c.repair_success {
                    Some(true) => "pass",
                    Some(false) => "fail",
                    None => "?",
                }
            ));
        }
        println!(
            "  {mark} {:<24} run={} steps={} tokens={}/{} latency={}ms{shape} {}{}{}",
            c.id,
            c.repetition,
            c.rounds,
            c.input_tokens,
            c.output_tokens,
            c.latency_ms,
            c.note,
            category,
            termination
        );
    }
    println!(
        "  {} {}/{} passed ({:.0}% completion, {:.0}% completion accuracy), avg {:.1} steps",
        console::style("→").bold(),
        report.passed_count(),
        report.total(),
        report.completion_rate() * 100.0,
        report.completion_accuracy() * 100.0,
        report.avg_rounds()
    );
    let self_recovered = report.self_recovered_edit_ids();
    if !self_recovered.is_empty() {
        println!(
            "  → self-recovered edit cases: {} ({})",
            self_recovered.len(),
            self_recovered.join(", ")
        );
    }
    let (repair_triggered, repair_succeeded) = report.repair_counts();
    if repair_triggered > 0 {
        println!(
            "  → engine repair: triggered in {repair_triggered} case(s), post-repair verification passed in {repair_succeeded}"
        );
    }
    // Headline agent-quality signal: the agent claimed "done" but the
    // independent check disagreed. Surface it prominently, never buried.
    let false_completions = report.false_completion_count();
    if false_completions > 0 {
        println!(
            "  {} false completion: {:.0}% ({}/{}) — claimed done, verification failed: {}",
            console::style("⚠").red().bold(),
            report.false_completion_rate() * 100.0,
            false_completions,
            report.total(),
            report.false_completion_case_ids().join(", ")
        );
    }
    // Behavior + engineering metrics: how the agent worked, not just whether it
    // passed. Validation rate is the leading indicator for false completions.
    println!(
        "  {} avg {:.1} tool calls · loop rate {:.0}% · validation rate {:.0}%",
        console::style("→").bold(),
        report.avg_tool_calls(),
        report.loop_rate() * 100.0,
        report.validation_rate() * 100.0,
    );
    // Runtime transparency: TTFF / silent gap from real event timestamps.
    // Never print fabricated zeros when no case recorded feedback.
    match (report.avg_ttff_ms(), report.max_silent_duration_ms()) {
        (Some(ttff), Some(silent)) => println!(
            "  {} avg TTFF {:.0}ms · max silent duration {}ms",
            console::style("→").bold(),
            ttff,
            silent
        ),
        (Some(ttff), None) => println!(
            "  {} avg TTFF {:.0}ms · silent duration n/a (<2 feedback events)",
            console::style("→").bold(),
            ttff
        ),
        (None, Some(silent)) => println!(
            "  {} TTFF n/a · max silent duration {}ms",
            console::style("→").bold(),
            silent
        ),
        (None, None) => println!(
            "  {} TTFF/silent duration n/a (no feedback events observed)",
            console::style("→").bold()
        ),
    }
    let breakdown = report.failure_breakdown();
    if !breakdown.is_empty() {
        let parts: Vec<String> = breakdown
            .iter()
            .map(|(category, count)| format!("{category}={count}"))
            .collect();
        println!(
            "  {} failures by first cause: {}",
            console::style("→").bold(),
            parts.join(" ")
        );
    }
    let unstable = report.unstable_case_ids();
    if !unstable.is_empty() {
        println!(
            "  {} unstable across repetitions: {}",
            console::style("!").yellow(),
            unstable.join(", ")
        );
    }
    if let Some(recovery) = report.recovery_rate() {
        println!(
            "  {} recovery rate {:.0}% (over injected-failure cases)",
            console::style("→").bold(),
            recovery * 100.0,
        );
    }
    // Headline composite: the single number the version-trend report tracks.
    let score = report.quality_score();
    println!(
        "  {} Agent Quality Score: {}/100 (over measured components)",
        console::style("★").yellow().bold(),
        score.score_100(),
    );
}

#[cfg(test)]
mod ablation_tests {
    use leveler_agent::StopReason;
    use leveler_eval::TerminationClass;

    #[test]
    fn ablation_overrides_flip_exactly_the_named_resolver_input() {
        // Plan gate defaults ON in the resolver; ablate turns it OFF.
        let (o, before, after) = super::ablation_overrides("explicit_plan").unwrap();
        assert!(before && !after);
        assert_eq!(o.explicit_plan, Some(false), "the named knob flipped OFF");
        // The single-variable contract: nothing else moved.
        assert_eq!(o.completion_evidence, None);
        assert_eq!(o.repeated_read_guard, None);
        assert_eq!(o.max_parallel_tools, None);

        // Safety rails ablate in the OFF direction (they default on).
        let (o, before, after) = super::ablation_overrides("completion_evidence").unwrap();
        assert!(before && !after);
        assert_eq!(o.completion_evidence, Some(false));

        let (o, before, after) = super::ablation_overrides("progress_guards").unwrap();
        assert!(before && !after);
        assert_eq!(o.repeated_read_guard, Some(false));

        // Legacy knob names keep working.
        let (legacy, ..) = super::ablation_overrides("require_explicit_plan").unwrap();
        assert_eq!(legacy.explicit_plan, Some(false));

        let err = super::ablation_overrides("not_a_knob").unwrap_err();
        assert!(
            err.to_string().contains("completion_evidence"),
            "unknown knob lists the valid ones: {err}"
        );
    }

    #[test]
    fn termination_is_independent_from_functional_correctness() {
        assert_eq!(
            super::termination_from_stop_reason(StopReason::Completed),
            TerminationClass::Completed
        );
        assert_eq!(
            super::termination_from_stop_reason(StopReason::BudgetExhausted),
            TerminationClass::BudgetLimited
        );
        assert_eq!(
            super::termination_from_stop_reason(StopReason::Blocked),
            TerminationClass::Blocked
        );
        assert_eq!(
            super::termination_from_stop_reason(StopReason::Stalled),
            TerminationClass::Incomplete
        );
    }

    /// The whole point of `--no-verify-gate` is that ONE variable changes: the
    /// post-edit verification plan is empty, so the gate (and the repair turn it
    /// drives) never runs. Every other knob must match the normal direct path,
    /// or a difference in results is not attributable to the gate.
    #[test]
    fn the_bare_spec_differs_from_the_direct_spec_only_in_verification() {
        let case = leveler_eval::EvaluationCase {
            id: "x".into(),
            name: "x".into(),
            repo: None,
            base_ref: None,
            files: Default::default(),
            recovery: false,
            task: "do the thing".into(),
            max_rounds: 40,
            relevant_paths: Vec::new(),
            required_impact_paths: Vec::new(),
            distractor_paths: Vec::new(),
            forbidden_edit_paths: Vec::new(),
            expected_outcome: Default::default(),
            expect: leveler_eval::ExpectCommand {
                program: "true".into(),
                args: vec![],
            },
        };

        let bare = leveler_engine::TaskSpec {
            runtime: leveler_engine::RuntimeTaskSpec {
                goal: case.task.clone(),
                kind: leveler_engine::ExecutionKind::Direct,
                continuation: leveler_agent::ContinuationPolicy::bounded(case.max_rounds),
                limits: leveler_agent::StepLimits::default(),
            },
            coding: leveler_engine::CodingTaskSpec {
                repository: std::path::PathBuf::from("/repo"),
                mode: leveler_execution::PermissionProfile::Assisted,
                sandbox: false,
                verification: leveler_verifier::VerificationPlan::default(),
                base_commit: None,
            },
        };

        assert!(
            bare.coding.verification.commands.is_empty(),
            "the ablated run must have nothing to verify with"
        );
        assert!(
            !bare.coding.verification.has_gates(),
            "an empty plan must report no gates, so the engine skips verification"
        );
        // The controls: identical to what the normal direct path passes.
        assert_eq!(
            bare.runtime.continuation,
            leveler_agent::ContinuationPolicy::bounded(40),
            "same round budget as the normal run"
        );
        assert_eq!(
            bare.runtime.limits,
            leveler_agent::StepLimits::default(),
            "same step limits as the normal run"
        );
        assert_eq!(bare.runtime.kind, leveler_engine::ExecutionKind::Direct);
    }
}

#[cfg(test)]
mod attribution_provenance_tests {
    //! `InfrastructureFailed` is an execution boundary, not a first cause:
    //! only structured provider/protocol provenance may book
    //! `ProviderProtocol`. These lock the mapping trio + the attribution
    //! chain, all from typed errors — never vendor message text.

    use super::{case_workspace, next_execution_seq};
    use leveler_eval::{FailureCategory, TerminationClass, TrajectorySignals};
    use leveler_model::{ModelError, ModelErrorKind};

    fn model_error(kind: ModelErrorKind) -> ModelError {
        ModelError::new(kind, "vendor text is irrelevant to attribution")
    }

    /// 1. A provider InvalidRequest is an infrastructure boundary AND a
    ///    provider-protocol first cause — the ripgrep C1.1a failure.
    #[test]
    fn invalid_request_is_infrastructure_and_provider_protocol() {
        let error = model_error(ModelErrorKind::InvalidRequest);
        assert_eq!(
            super::termination_from_model_error(&error),
            TerminationClass::InfrastructureFailed
        );
        assert_eq!(
            super::infrastructure_cause_from_model_error(&error),
            Some(FailureCategory::ProviderProtocol)
        );
    }

    /// 2. A non-model app-layer death is the framework's own failure — never
    ///    provider protocol, even though its termination is InfrastructureFailed.
    #[test]
    fn non_model_app_error_is_not_provider_protocol() {
        let error = leveler_app::AppError::Engine("engine internals fell over".into());
        assert_eq!(
            super::termination_from_app_error(&error),
            TerminationClass::InfrastructureFailed
        );
        assert_eq!(
            super::infrastructure_cause_from_app_error(&error),
            Some(FailureCategory::Runtime)
        );
        // Setup-class app errors book as Environment, still not provider protocol.
        let missing = leveler_app::AppError::NotFound("no such session".into());
        assert_eq!(
            super::infrastructure_cause_from_app_error(&missing),
            Some(FailureCategory::Environment)
        );
    }

    /// 3. A non-model engine-layer death: same policy.
    #[test]
    fn non_model_engine_error_is_not_provider_protocol() {
        let error = leveler_engine::EngineError::Config("bad engine config".into());
        assert_eq!(
            super::termination_from_engine_error(&error),
            TerminationClass::InfrastructureFailed
        );
        assert_eq!(
            super::infrastructure_cause_from_engine_error(&error),
            Some(FailureCategory::Runtime)
        );
    }

    /// 4. Reachability failures are environment, not protocol: the wire
    ///    contract was never even exercised.
    #[test]
    fn unreachable_provider_is_environment_not_provider_protocol() {
        for kind in [
            ModelErrorKind::ProviderUnavailable,
            ModelErrorKind::Transport,
            ModelErrorKind::Timeout,
            ModelErrorKind::Auth,
        ] {
            let error = model_error(kind);
            assert_eq!(
                super::termination_from_model_error(&error),
                TerminationClass::InfrastructureFailed,
                "{kind:?} stays an infrastructure boundary"
            );
            assert_eq!(
                super::infrastructure_cause_from_model_error(&error),
                Some(FailureCategory::Environment),
                "{kind:?} must not be booked as provider_protocol"
            );
        }
        // Non-infrastructure terminations carry no infrastructure cause.
        assert_eq!(
            super::infrastructure_cause_from_model_error(&model_error(ModelErrorKind::RateLimit)),
            None
        );
        assert_eq!(
            super::infrastructure_cause_from_model_error(&model_error(ModelErrorKind::Cancelled)),
            None
        );
    }

    /// 5. The original ripgrep shape end to end: a provider InvalidRequest
    ///    reaching attribution through the app-error path still books
    ///    provider_protocol, not the trajectory's localization shape.
    #[test]
    fn ripgrep_invalid_request_still_attributes_provider_protocol() {
        let error = leveler_app::AppError::Agent(leveler_agent::AgentError::Model(model_error(
            ModelErrorKind::InvalidRequest,
        )));
        let cause = super::infrastructure_cause_from_app_error(&error);
        let localization_shaped = TrajectorySignals {
            tool_calls: 4,
            ..Default::default()
        };
        assert_eq!(
            leveler_eval::attribute_failure(false, false, cause, &localization_shaped),
            Some(FailureCategory::ProviderProtocol)
        );
    }

    /// C2-R1 — one workspace per case *execution*, not per case.
    ///
    /// The path was `<case>-<pid>`, and `run_eval_case` wipes its directory on
    /// entry, so three repetitions in one process each deleted the previous
    /// one's tree — even under `LEVELER_EVAL_KEEP_WORKSPACE=1`, which only
    /// governs cleanup at the end. Invisible while a run is scored by its exit
    /// status; fatal as soon as anything needs the tree a particular run left
    /// behind, which per-obligation implementation coverage does.
    #[test]
    fn repetitions_of_one_case_do_not_share_a_workspace() {
        let paths: Vec<_> = (1..=3)
            .map(|rep| case_workspace("n3-caller", 4242, 1, rep))
            .collect();
        let unique: std::collections::HashSet<_> = paths.iter().collect();
        assert_eq!(unique.len(), paths.len(), "repetitions collided: {paths:?}");
        for (rep, path) in (1..=3).zip(&paths) {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.contains("n3-caller"),
                "case id must stay legible: {name}"
            );
            assert!(name.contains("4242"), "pid must stay legible: {name}");
            assert!(
                name.ends_with(&format!("-r{rep}")),
                "repetition must be recoverable from the path: {name}"
            );
        }
    }

    /// `compare` runs two models and `ablate` runs control and treatment, each
    /// re-entering `run_eval` in the same process. Keying on case + pid +
    /// repetition would still collide there, which is why identity is per
    /// execution.
    #[test]
    fn separate_run_invocations_do_not_share_a_workspace() {
        assert_ne!(
            case_workspace("n3-caller", 4242, 1, 1),
            case_workspace("n3-caller", 4242, 2, 1),
            "two run_eval invocations must not reuse one workspace"
        );
        assert_ne!(
            case_workspace("n3-caller", 4242, 1, 1),
            case_workspace("n4-contract", 4242, 1, 1)
        );
    }

    /// The sequence is monotonic per process, so every execution gets a fresh
    /// identity without call sites having to coordinate.
    #[test]
    fn execution_sequence_never_repeats() {
        let seen: std::collections::HashSet<u64> = (0..8).map(|_| next_execution_seq()).collect();
        assert_eq!(seen.len(), 8, "execution sequence handed out a duplicate");
    }

    /// The isolation is real on disk, not only in the name: a kept tree from
    /// one execution must survive the next execution's entry wipe. This is the
    /// exact behaviour that destroyed the first R1 replay's data.
    #[test]
    fn an_earlier_executions_kept_tree_survives_the_next_one() {
        let first = case_workspace("iso-probe", std::process::id(), 901, 1);
        let second = case_workspace("iso-probe", std::process::id(), 902, 2);
        for path in [&first, &second] {
            std::fs::remove_dir_all(path).ok();
        }
        std::fs::create_dir_all(&first).unwrap();
        std::fs::write(first.join("final.txt"), "run one output\n").unwrap();

        // What `run_eval_case` does on entry for the next execution.
        let _ = std::fs::remove_dir_all(&second);
        std::fs::create_dir_all(&second).unwrap();

        assert!(
            first.join("final.txt").exists(),
            "the previous execution's kept tree was destroyed"
        );
        assert_eq!(
            std::fs::read_to_string(first.join("final.txt")).unwrap(),
            "run one output\n"
        );
        for path in [&first, &second] {
            std::fs::remove_dir_all(path).ok();
        }
    }

    /// C2.3C §31 — a cloned workspace must not carry a path back to the
    /// fixture it came from. An eval whose answer key is reachable from inside
    /// the workspace measures the guard, not the agent.
    #[test]
    fn a_cloned_eval_workspace_keeps_no_pointer_to_its_source() {
        let base = std::env::temp_dir().join(format!(
            "leveler-eval-origin-{}",
            std::process::id() as u64 * 31 + 7
        ));
        std::fs::remove_dir_all(&base).ok();
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), "hello\n").unwrap();
        let run = |cwd: &std::path::Path, args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git")
        };
        run(&src, &["init", "-q"]);
        run(&src, &["config", "user.email", "a@b"]);
        run(&src, &["config", "user.name", "a"]);
        run(&src, &["add", "-A"]);
        run(&src, &["commit", "-qm", "seed"]);

        std::process::Command::new("git")
            .args(["clone", "--local", "--quiet"])
            .arg(&src)
            .arg(&dst)
            .output()
            .expect("clone");
        let before = run(&dst, &["remote", "-v"]);
        assert!(
            String::from_utf8_lossy(&before.stdout).contains("src"),
            "fixture precondition: a fresh clone names its origin"
        );

        // What the eval now does to it.
        run(&dst, &["remote", "remove", "origin"]);

        let after = run(&dst, &["remote", "-v"]);
        assert!(
            String::from_utf8_lossy(&after.stdout).trim().is_empty(),
            "the workspace must not name its source: {}",
            String::from_utf8_lossy(&after.stdout)
        );
        let config = std::fs::read_to_string(dst.join(".git/config")).unwrap();
        assert!(
            !config.contains(src.to_string_lossy().as_ref()),
            "the source path must not survive in .git/config: {config}"
        );

        // C2.3C §15/§16 J — cutting the breadcrumb must not cut history. Real
        // navigation uses `git log`/`show`/`blame` to ask why code looks the
        // way it does; an anti-gaming fix that took those away would be
        // measuring a crippled agent.
        let log = run(&dst, &["log", "--oneline"]);
        assert!(log.status.success(), "git log must still work");
        assert!(
            String::from_utf8_lossy(&log.stdout).contains("seed"),
            "history must survive origin removal: {}",
            String::from_utf8_lossy(&log.stdout)
        );
        let show = run(&dst, &["show", "HEAD:a.txt"]);
        assert!(show.status.success(), "git show must still work");
        assert_eq!(String::from_utf8_lossy(&show.stdout).trim(), "hello");
        let blame = run(&dst, &["blame", "--porcelain", "a.txt"]);
        assert!(blame.status.success(), "git blame must still work");
        let status = run(&dst, &["status", "--porcelain"]);
        assert!(status.status.success(), "git status must still work");

        std::fs::remove_dir_all(&base).ok();
    }
}
