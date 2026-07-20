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
use crate::common::{build_approver, resolve_model};
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
            let mode = match (direct, no_verify_gate) {
                (true, true) => "direct-no-verify-gate",
                (true, false) => "direct",
                _ => "orchestrated",
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
                        "orchestrated",
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
            let mode = if direct { "direct" } else { "orchestrated" };
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
    }
}

/// Flip one boolean policy knob in place; returns `(before, after)` for the
/// run banner. The knob names mirror the `configs/policies/*.yaml` fields.
/// Build the ablated arm's overrides for one knob: exactly one resolver input
/// flipped away from its default. Legacy `require_*` names stay as aliases so
/// existing scripts keep working.
fn ablation_overrides(knob: &str) -> anyhow::Result<(ExecutionOverrides, bool, bool)> {
    let mut o = ExecutionOverrides::default();
    let (before, after) = match knob {
        "explicit_plan" | "require_explicit_plan" => {
            o.explicit_plan = Some(true);
            (false, true)
        }
        "step_summary" | "require_step_summary" => {
            o.step_summary_every = Some(6);
            (false, true)
        }
        "completion_evidence" | "require_completion_evidence" => {
            o.completion_evidence = Some(false);
            (true, false)
        }
        "repeated_read_guard" => {
            o.repeated_read_guard = Some(false);
            (true, false)
        }
        _ => anyhow::bail!(
            "unknown knob `{knob}` — expected one of: explicit_plan, \
             step_summary, completion_evidence, repeated_read_guard"
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
    let mut command = std::process::Command::new("git");
    command.args(["rev-parse", "HEAD"]);
    scrub_command_env(&mut command);
    let out = command.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn scrub_command_env(command: &mut std::process::Command) {
    command.env_clear();
    for (name, value) in leveler_core::environment().vars_os() {
        if !name
            .to_str()
            .is_some_and(leveler_execution::is_credential_env_name)
        {
            command.env(name, value);
        }
    }
}

/// Run every case against one model in an isolated temp repo.
///
/// `checkpoint` (when set) receives one JSON line per finished case. A Hard-set
/// run is hours long; without it, an interrupt anywhere before the final write
/// loses every completed case. The file is append-only and self-describing, so
/// a killed run's cases can still be recovered and compared.
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
) {
    let engine = match app
        .engine_for(
            model,
            PermissionProfile::Assisted,
            false,
            build_approver(true),
            Arc::new(leveler_agent::AutoClarify),
        )
        .await
    {
        Ok(engine) => engine,
        Err(e) => {
            let termination = termination_from_app_error(&e);
            return (None, false, 0, format!("engine: {e}"), termination);
        }
    };
    let spec = leveler_engine::TaskSpec {
        repository: app.layout.repo_root.clone(),
        goal: case.task.clone(),
        mode: PermissionProfile::Assisted,
        sandbox: false,
        kind: leveler_engine::ExecutionKind::Direct,
        continuation: leveler_agent::ContinuationPolicy::bounded(case.max_rounds),
        limits: leveler_agent::StepLimits::default(),
        // THE ablated variable: an empty plan means there is nothing to verify.
        verification: leveler_verifier::VerificationPlan::default(),
    };
    let session_id = match engine.create_task(&spec).await {
        Ok(id) => id,
        Err(e) => {
            let termination = termination_from_engine_error(&e);
            return (None, false, 0, format!("session: {e}"), termination);
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
            )
        }
        Err(e) => {
            let termination = termination_from_engine_error(&e);
            (
                Some(session_id),
                false,
                0,
                format!("error: {e}"),
                termination,
            )
        }
    }
}

fn termination_from_stop_reason(reason: StopReason) -> leveler_eval::TerminationClass {
    match reason {
        StopReason::Completed
        | StopReason::Answered
        | StopReason::CompletedUnverified
        | StopReason::CloseoutForced => leveler_eval::TerminationClass::Completed,
        StopReason::BudgetExhausted => leveler_eval::TerminationClass::BudgetLimited,
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
    };

    // Materialize the workspace. Two modes:
    //  - synthetic: an empty repo seeded entirely from `case.files`.
    //  - repo:      clone a real git repo, then overlay `case.files` on top so
    //               the agent must locate the relevant code in a full codebase.
    let dir = std::env::temp_dir().join(format!("leveler-eval-{}-{}", case.id, std::process::id()));
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

    // Run the orchestrated agent in the case workspace.
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
    let mut collector = crate::eval_signals::SignalCollector::new(case.files.keys().cloned());
    let (session_id, completed, rounds, mut note, termination) = if no_verify_gate {
        // Ablation: the SAME direct loop with ONE variable removed — the
        // post-edit verification gate and the repair turn it drives. The model's
        // own "done" is final. `expect` still decides pass/fail independently,
        // so this measures how often verify→repair rescues a run the model
        // would otherwise have gotten wrong.
        run_bare_case(&app, model, case, &mut collector).await
    } else if direct {
        // Ablation: the naive direct tool loop, no orchestration scaffold.
        match app.create_session(model, &case.task).await {
            Ok(session_id) => {
                let outcome = app
                    .run_in_session_bounded(
                        &session_id,
                        model,
                        PermissionProfile::Assisted,
                        &case.task,
                        build_approver(true),
                        false,
                        &mut |e| collector.observe_agent(&e),
                        CancellationToken::new(),
                        case.max_rounds,
                    )
                    .await;
                match outcome {
                    Ok(o) => {
                        let termination = termination_from_stop_reason(o.stop_reason);
                        (
                            Some(session_id),
                            o.stop_reason == StopReason::Completed,
                            o.rounds,
                            format!("{:?}", o.stop_reason),
                            termination,
                        )
                    }
                    Err(e) => {
                        let termination = termination_from_app_error(&e);
                        (
                            Some(session_id),
                            false,
                            0,
                            format!("error: {e}"),
                            termination,
                        )
                    }
                }
            }
            Err(e) => {
                let termination = termination_from_app_error(&e);
                (None, false, 0, format!("session: {e}"), termination)
            }
        }
    } else {
        let outcome = app
            .orchestrate_task_bounded(
                model,
                PermissionProfile::Assisted,
                &case.task,
                build_approver(true),
                Arc::new(leveler_agent::AutoClarify),
                false,
                &mut |e| collector.observe_engine(e),
                CancellationToken::new(),
                None,
                case.max_rounds,
            )
            .await;
        match outcome {
            Ok((session_id, report)) => {
                let termination = termination_from_report(report.outcome, report.stop_reason);
                (
                    Some(session_id),
                    report.outcome.is_success(),
                    report.rounds,
                    format!("{:?}", report.outcome),
                    termination,
                )
            }
            Err(e) => {
                let termination = termination_from_app_error(&e);
                (None, false, 0, format!("error: {e}"), termination)
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

    let _ = std::fs::remove_dir_all(&dir);
    // First-cause attribution receives the structured budget marker rather
    // than parsing a debug-formatted outcome note.
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
        failure_category: leveler_eval::attribute_failure(completed, expect_passed, &signals),
        failure_source: (!(completed && expect_passed))
            .then_some(leveler_eval::FailureSource::Auto),
        note,
        verification_evidence: Some(leveler_eval::VerificationEvidence {
            program: case.expect.program.clone(),
            args: case.expect.args.clone(),
            passed: expect_passed,
            exit_code: verification_exit_code,
        }),
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
        println!(
            "  {mark} {:<24} run={} steps={} tokens={}/{} latency={}ms {}{}{}",
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
        "  {} {}/{} passed ({:.0}% completion), avg {:.1} steps",
        console::style("→").bold(),
        report.passed_count(),
        report.total(),
        report.completion_rate() * 100.0,
        report.avg_rounds()
    );
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
}

#[cfg(test)]
mod ablation_tests {
    use leveler_agent::StopReason;
    use leveler_eval::TerminationClass;

    #[test]
    fn ablation_overrides_flip_exactly_the_named_resolver_input() {
        let (o, before, after) = super::ablation_overrides("explicit_plan").unwrap();
        assert!(!before && after);
        assert_eq!(o.explicit_plan, Some(true), "the named knob flipped ON");
        // The single-variable contract: nothing else moved.
        assert_eq!(o.step_summary_every, None);
        assert_eq!(o.completion_evidence, None);
        assert_eq!(o.repeated_read_guard, None);
        assert_eq!(o.max_parallel_tools, None);

        // Safety rails ablate in the OFF direction (they default on).
        let (o, before, after) = super::ablation_overrides("completion_evidence").unwrap();
        assert!(before && !after);
        assert_eq!(o.completion_evidence, Some(false));

        // Legacy knob names keep working.
        let (legacy, ..) = super::ablation_overrides("require_step_summary").unwrap();
        assert_eq!(legacy.step_summary_every, Some(6));

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
            task: "do the thing".into(),
            max_rounds: 40,
            expect: leveler_eval::ExpectCommand {
                program: "true".into(),
                args: vec![],
            },
        };

        let bare = leveler_engine::TaskSpec {
            repository: std::path::PathBuf::from("/repo"),
            goal: case.task.clone(),
            mode: leveler_execution::PermissionProfile::Assisted,
            sandbox: false,
            kind: leveler_engine::ExecutionKind::Direct,
            continuation: leveler_agent::ContinuationPolicy::bounded(case.max_rounds),
            limits: leveler_agent::StepLimits::default(),
            verification: leveler_verifier::VerificationPlan::default(),
        };

        assert!(
            bare.verification.commands.is_empty(),
            "the ablated run must have nothing to verify with"
        );
        assert!(
            !bare.verification.has_gates(),
            "an empty plan must report no gates, so the engine skips verification"
        );
        // The controls: identical to what the normal direct path passes.
        assert_eq!(
            bare.continuation,
            leveler_agent::ContinuationPolicy::bounded(40),
            "same round budget as the normal run"
        );
        assert_eq!(
            bare.limits,
            leveler_agent::StepLimits::default(),
            "same step limits as the normal run"
        );
        assert_eq!(bare.kind, leveler_engine::ExecutionKind::Direct);
    }
}
