//! Mechanical completion accounting for the structured requirement set.
//!
//! The model declares requirements at `commit_task` and changes them through
//! `declare_scope`; the runtime never re-derives them from prose. This gate
//! enforces only what the declaration and the execution record can prove:
//!
//! - every active deliverable has one account, keyed by its stable id;
//! - an account declared `not_done` refuses completion;
//! - an `artifact`, `artifact_absent`, `content` or `command` requirement is
//!   checked against the current workspace or the recorded command runs;
//! - a `tests` requirement declares its exact test command in `target` and
//!   needs a passing recorded run of it after the last change; the runtime does
//!   not recognise a runner's name, and command success without a structured
//!   test result is reported as unverified test coverage, never as Verified;
//! - a `named` requirement is accepted with an explicit unverified boundary.
//!
//! The model's `done` claim never overrides a fact the runtime can observe.

use crate::executor::goal_verification::{PendingCommand, VerificationRun};
use leveler_lifecycle::{
    RequirementItem, RequirementKind, VerificationRecord, VerificationRecordState,
    VerificationStatus,
};

/// The floor for accounting capacity. A goal with fewer deliverables may still
/// carry this many entries; the caller raises it to the number of owed items so
/// a legitimate long task can account for every one.
pub(crate) const MIN_ACCOUNTING_ENTRIES: usize = 20;

/// The accounting capacity for a requirement set.
pub(crate) fn entry_limit(owed: usize) -> usize {
    owed.max(MIN_ACCOUNTING_ENTRIES)
}

/// How the model says one requirement stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountStatus {
    Done,
    NotDone,
    /// Delivered work the runtime cannot prove. Never an exemption from a fact
    /// the runtime *can* prove.
    Unverifiable,
}

/// One account from `update_goal(complete).requirements`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequirementAccount {
    pub id: String,
    pub status: AccountStatus,
    pub evidence: String,
}

/// What the runtime could observe about one artifact path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactState {
    Present,
    Absent,
    /// No workspace, an absolute path outside it, or a path that escapes it.
    Unknown,
}

/// What the runtime could observe about a content literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiteralState {
    /// A line of the target file holds the literal.
    Present,
    /// The target exists and no line holds the literal.
    Absent,
    /// The file could not be read (no workspace, outside it, or unreadable).
    Unknown,
}

/// Everything [`decide`] needs. Pure data so the gate is unit-testable.
pub(crate) struct ScopeFacts<'a> {
    pub requirements: &'a [RequirementItem],
    pub accounting: Option<&'a [RequirementAccount]>,
    pub runs: &'a [VerificationRun],
    /// Commands this task started and has not seen a result for. They are not
    /// evidence in either direction; naming one as satisfied is refused for
    /// the real reason (no result yet) instead of "no run".
    pub pending: &'a [PendingCommand],
    pub mutation_ops: u64,
    pub artifact: &'a dyn Fn(&str) -> ArtifactState,
    /// Reads the workspace for a `content` requirement:
    /// `(target path, literal) -> state`.
    pub literal: &'a dyn Fn(&str, &str) -> LiteralState,
}

/// The gate's answer for one `update_goal(complete)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopeDecision {
    /// Completion may proceed. `status` is `None` when nothing was owed;
    /// otherwise `Verified` only when every owed requirement has a mechanical
    /// record and `Unverified` when at least one rests on the model's account.
    /// `warnings` and `records` are persisted on the terminal.
    Accept {
        status: Option<VerificationStatus>,
        warnings: Vec<String>,
        records: Vec<VerificationRecord>,
    },
    /// Completion is refused; the goal stays active and the model sees exactly
    /// which requirement ids it did not account for.
    Refuse(String),
}

/// Parse the `requirements` accounting array from `update_goal`.
///
/// The entries are keyed by the stable requirement id, not by restating the
/// requirement text: identity is what makes an account unambiguous after an
/// amend or replace.
pub(crate) fn parse_accounting(
    arguments: &serde_json::Value,
    owed: usize,
) -> Result<Option<Vec<RequirementAccount>>, String> {
    let Some(value) = arguments.get("requirements") else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(
            "`requirements` must be an array of {id, status, evidence} objects".to_string(),
        );
    };
    let limit = entry_limit(owed);
    if items.len() > limit {
        return Err(format!(
            "`requirements` may list at most {limit} entries; account for the active \
             deliverable ids only"
        ));
    }
    let mut accounts = Vec::with_capacity(items.len());
    for item in items {
        let id = item
            .get("id")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.chars().count() <= 64)
            .ok_or_else(|| {
                "each `requirements` entry needs the requirement `id` (for example \"R1\")"
                    .to_string()
            })?;
        let status = match item.get("status").and_then(|value| value.as_str()) {
            Some("done") => AccountStatus::Done,
            Some("not_done") => AccountStatus::NotDone,
            Some("unverifiable") => AccountStatus::Unverifiable,
            other => {
                return Err(format!(
                    "`requirements[].status` must be \"done\", \"not_done\" or \"unverifiable\" \
                     (got {})",
                    other
                        .map(|status| format!("\"{status}\""))
                        .unwrap_or_else(|| "no status".to_string())
                ));
            }
        };
        let evidence = item
            .get("evidence")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.chars().count() <= 1000)
            .ok_or_else(|| {
                "each `requirements` entry needs non-empty `evidence` (up to 1000 characters)"
                    .to_string()
            })?;
        accounts.push(RequirementAccount {
            id: id.to_string(),
            status,
            evidence: evidence.to_string(),
        });
    }
    Ok(Some(accounts))
}

/// Decide whether the completion accounts for every active deliverable.
pub(crate) fn decide(facts: &ScopeFacts<'_>) -> ScopeDecision {
    let owed: Vec<&RequirementItem> = facts
        .requirements
        .iter()
        .filter(|item| item.is_owed())
        .collect();
    if owed.is_empty() {
        return ScopeDecision::Accept {
            status: None,
            warnings: Vec::new(),
            records: Vec::new(),
        };
    }
    let accounting = facts.accounting.unwrap_or(&[]);
    let mut refusals: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut records: Vec<VerificationRecord> = Vec::with_capacity(owed.len());

    for item in &owed {
        let Some(account) = accounting.iter().find(|account| account.id == item.id) else {
            refusals.push(format!(
                "`{}` [{}] `{}` has no account; add an entry with id `{}`",
                item.id,
                item.kind.label(),
                clip(&item.text, 120),
                item.id
            ));
            continue;
        };
        let duplicate = accounting
            .iter()
            .filter(|other| other.id == item.id)
            .count()
            > 1;
        if duplicate {
            refusals.push(format!(
                "requirement `{}` is accounted more than once; keep one entry per id",
                item.id
            ));
            continue;
        }
        match account.status {
            AccountStatus::NotDone => refusals.push(format!(
                "`{}` [{}] `{}` is accounted not_done ({})",
                item.id,
                item.kind.label(),
                clip(&item.text, 120),
                clip(&account.evidence, 200)
            )),
            AccountStatus::Unverifiable => {
                if let Some(refusal) = contradicted(facts, item, account) {
                    refusals.push(refusal);
                } else {
                    let detail = format!(
                        "explicitly unverifiable ({}); the completion carries an unverified \
                         boundary",
                        clip(&account.evidence, 200)
                    );
                    warnings.push(format!(
                        "requirement `{}` [{}] `{}` is {detail}",
                        item.id,
                        item.kind.label(),
                        clip(&item.text, 120)
                    ));
                    records.push(record(
                        item,
                        VerificationRecordState::Unverified,
                        detail,
                        None,
                        facts.mutation_ops,
                    ));
                }
            }
            AccountStatus::Done => match check_mechanical(facts, item, account) {
                Mechanical::Failed(refusal) => refusals.push(refusal),
                Mechanical::Unverified(warning, run) => {
                    warnings.push(warning.clone());
                    records.push(record(
                        item,
                        VerificationRecordState::Unverified,
                        warning,
                        run,
                        facts.mutation_ops,
                    ));
                }
                Mechanical::Satisfied(run) => {
                    let detail = match run {
                        Some(run) => format!(
                            "verified against the execution record: `{}` {} at workspace version {}",
                            run.label,
                            exit_description(run),
                            run.mutation_ops
                        ),
                        None => "verified against the current workspace".to_string(),
                    };
                    records.push(record(
                        item,
                        VerificationRecordState::Verified,
                        detail,
                        run,
                        facts.mutation_ops,
                    ));
                }
                Mechanical::NotCheckable => {
                    let detail = "reported done but has no mechanical check; the completion rests \
                                  on the model's account, not runtime evidence";
                    warnings.push(format!(
                        "requirement `{}` [{}] `{}` is {detail}",
                        item.id,
                        item.kind.label(),
                        clip(&item.text, 120)
                    ));
                    records.push(record(
                        item,
                        VerificationRecordState::Unverified,
                        detail.to_string(),
                        None,
                        facts.mutation_ops,
                    ));
                }
            },
        }
    }

    if !refusals.is_empty() {
        return ScopeDecision::Refuse(refusal_message(&refusals));
    }
    let status = if records
        .iter()
        .all(|record| record.state == VerificationRecordState::Verified)
    {
        VerificationStatus::Verified
    } else {
        VerificationStatus::Unverified
    };
    ScopeDecision::Accept {
        status: Some(status),
        warnings,
        records,
    }
}

/// One requirement's place in the accepted completion. Evidence fields come
/// from the cited run when the requirement had one; otherwise the record says
/// plainly what the runtime could not check.
fn record(
    item: &RequirementItem,
    state: VerificationRecordState,
    detail: String,
    run: Option<&VerificationRun>,
    workspace_version: u64,
) -> VerificationRecord {
    VerificationRecord {
        requirement_id: item.id.clone(),
        kind: item.kind.label().to_string(),
        state,
        detail,
        call_id: run.and_then(|run| run.call_id.clone()),
        command: run.map(|run| run.label.clone()),
        exit_code: run.and_then(|run| run.exit_code),
        workspace_version: run.map_or(workspace_version, |run| run.mutation_ops),
        model_step: run.map_or(0, |run| run.model_step),
    }
}

fn exit_description(run: &VerificationRun) -> String {
    match run.exit_code {
        Some(code) => format!("exited {code}"),
        None => "succeeded without a reported exit status".to_string(),
    }
}

/// A contradiction between the model's `unverifiable` boundary and a fact the
/// runtime can observe. Only kinds with a mechanical check can contradict.
fn contradicted(
    facts: &ScopeFacts<'_>,
    item: &RequirementItem,
    account: &RequirementAccount,
) -> Option<String> {
    match check_mechanical(facts, item, account) {
        Mechanical::Failed(refusal) => Some(refusal),
        _ => None,
    }
}

enum Mechanical<'a> {
    /// The runtime observed the declared fact. The run is present when the
    /// check was an execution record.
    Satisfied(Option<&'a VerificationRun>),
    NotCheckable,
    /// The fact is observable and contradicts the account.
    Failed(String),
    /// Checked where possible, but the observation boundary is unknown or the
    /// available evidence is only partial. The run, when present, is the real
    /// execution the record rests on.
    Unverified(String, Option<&'a VerificationRun>),
}

fn check_mechanical<'a>(
    facts: &ScopeFacts<'a>,
    item: &RequirementItem,
    account: &RequirementAccount,
) -> Mechanical<'a> {
    match item.kind {
        RequirementKind::Artifact => {
            let Some(target) = item.target.as_deref() else {
                return Mechanical::Failed(format!(
                    "requirement `{}` [artifact] names no path; the declaration is missing `target`",
                    item.id
                ));
            };
            match (facts.artifact)(target) {
                ArtifactState::Present => Mechanical::Satisfied(None),
                ArtifactState::Absent => Mechanical::Failed(format!(
                    "`{}` [artifact] `{}` is accounted done, but `{target}` does not exist in \
                     the workspace and no recorded change created it",
                    item.id,
                    clip(&item.text, 120)
                )),
                ArtifactState::Unknown => Mechanical::Unverified(
                    format!(
                        "requirement `{}` [artifact] `{}` is accounted done, but the runtime could \
                         not inspect `{target}`; it is unverified",
                        item.id,
                        clip(&item.text, 120)
                    ),
                    None,
                ),
            }
        }
        RequirementKind::ArtifactAbsent => {
            let Some(target) = item.target.as_deref() else {
                return Mechanical::Failed(format!(
                    "requirement `{}` [absent-artifact] names no path; the declaration is \
                     missing `target`",
                    item.id
                ));
            };
            match (facts.artifact)(target) {
                ArtifactState::Absent => Mechanical::Satisfied(None),
                ArtifactState::Present => Mechanical::Failed(format!(
                    "`{}` [absent-artifact] `{}` is accounted deleted, but `{target}` still exists",
                    item.id,
                    clip(&item.text, 120)
                )),
                ArtifactState::Unknown => Mechanical::Unverified(
                    format!(
                        "requirement `{}` [absent-artifact] `{}` is accounted deleted, but the \
                         runtime could not inspect `{target}`; it is unverified",
                        item.id,
                        clip(&item.text, 120)
                    ),
                    None,
                ),
            }
        }
        RequirementKind::Content => {
            let target = item.target.as_deref().unwrap_or("");
            match (facts.literal)(target, &item.text) {
                LiteralState::Present => Mechanical::Satisfied(None),
                LiteralState::Absent => Mechanical::Failed(format!(
                    "`{}` [content] `{}` is accounted done, but it does not appear as a line \
                     of `{target}`. `content` compares the requirement's `text` to a file \
                     line; if the record holds a description instead of the literal, amend \
                     `text` with declare_scope.",
                    item.id,
                    clip(&item.text, 120)
                )),
                LiteralState::Unknown => Mechanical::Unverified(
                    format!(
                        "requirement `{}` [content] `{}` is accounted done, but the runtime could \
                         not read `{target}`; it is unverified",
                        item.id,
                        clip(&item.text, 120)
                    ),
                    None,
                ),
            }
        }
        RequirementKind::Command => {
            let Some(target) = item.target.as_deref() else {
                return Mechanical::Failed(format!(
                    "requirement `{}` [command] names no command; the declaration is missing \
                     `target`",
                    item.id
                ));
            };
            match successful_run(facts, target, &account.evidence) {
                RunCheck::Found(run) => Mechanical::Satisfied(Some(run)),
                RunCheck::Pending(started) => {
                    Mechanical::Failed(pending_refusal(item, target, started))
                }
                RunCheck::Stale(run) => Mechanical::Failed(format!(
                    "`{}` [command] `{target}` last passed before the most recent change \
                     (step {}); run it against the current workspace",
                    item.id, run.model_step
                )),
                RunCheck::Failed(run) => Mechanical::Failed(format!(
                    "`{}` [command] `{target}` was cited but its last run failed (step {})",
                    item.id, run.model_step
                )),
                RunCheck::Missing => Mechanical::Failed(format!(
                    "`{}` [command] `{target}` is accounted done, but no successful run after \
                     the last change matches it",
                    item.id
                )),
            }
        }
        RequirementKind::Tests => {
            // A requirement persisted before `tests` required a `target`
            // cannot be matched to a run. It stays finishable at the explicit
            // unverified boundary instead of blocking a legally recorded goal.
            let Some(target) = item.target.as_deref() else {
                return Mechanical::Unverified(
                    format!(
                        "requirement `{}` [tests] `{}` was recorded without a declared test \
                         command, so no recorded run can be matched to it; the completion \
                         carries an unverified boundary",
                        item.id,
                        clip(&item.text, 120)
                    ),
                    None,
                );
            };
            match successful_run(facts, target, &account.evidence) {
                // The runtime recorded the declared command and it exited 0.
                // That is command success, not test-execution success: the
                // execution record carries no structured test result, so a
                // command that ran zero tests is indistinguishable here. The
                // requirement is complete but its coverage stays unverified.
                RunCheck::Found(run) => Mechanical::Unverified(
                    format!(
                        "`{}` [tests] `{}` {} at workspace version {} (step {}), but the runtime \
                         observed no structured test result: command success does not establish \
                         that any test executed, so the test coverage remains unverified",
                        item.id,
                        run.label,
                        exit_description(run),
                        run.mutation_ops,
                        run.model_step
                    ),
                    Some(run),
                ),
                RunCheck::Pending(started) => {
                    Mechanical::Failed(pending_refusal(item, target, started))
                }
                RunCheck::Stale(run) => Mechanical::Failed(format!(
                    "`{}` [tests] `{target}` last passed before the most recent change \
                     (step {}); run the declared test command against the current workspace",
                    item.id, run.model_step
                )),
                RunCheck::Failed(run) => Mechanical::Failed(format!(
                    "`{}` [tests] `{target}` was cited but its last run failed (step {})",
                    item.id, run.model_step
                )),
                RunCheck::Missing => Mechanical::Failed(format!(
                    "`{}` [tests] is accounted done, but no passing run of the declared test \
                     command `{target}` was recorded after the last change. Put `call_id:<id>` \
                     or the exact command on its own line of `evidence`, and run the declared \
                     command now if it did not run.",
                    item.id
                )),
            }
        }
        RequirementKind::Named
        | RequirementKind::Forbidden
        | RequirementKind::Baseline
        | RequirementKind::Preparation
        | RequirementKind::Constraint => Mechanical::NotCheckable,
    }
}

/// A command that has only started is not a satisfied requirement: its result
/// is unknown. Name the real missing fact and the exact action that resolves it.
fn pending_refusal(item: &RequirementItem, command: &str, started: &PendingCommand) -> String {
    let task = match started.task_id.as_deref() {
        Some(id) => format!(" as background task {id}"),
        None => String::new(),
    };
    let wait = match started.task_id.as_deref() {
        Some(id) => format!("wait_task with task_id {id}"),
        None => "wait_task for that task".to_string(),
    };
    format!(
        "`{}` [{}] `{command}` has only STARTED{task} and reported no result yet. Wait for it \
         ({wait}) and account for it again once it exits.",
        item.id,
        item.kind.label()
    )
}

enum RunCheck<'a> {
    Found(&'a VerificationRun),
    /// The command started and has no result yet.
    Pending(&'a PendingCommand),
    Stale(&'a VerificationRun),
    Failed(&'a VerificationRun),
    Missing,
}

/// A successful, current run of the command the requirement names.
fn successful_run<'a>(facts: &ScopeFacts<'a>, command: &str, evidence: &str) -> RunCheck<'a> {
    let cited = evidence.lines().find_map(|line| {
        let line = line.trim();
        (!line.is_empty())
            .then(|| crate::executor::goal_verification::cited_run_in(line, facts.runs))
            .flatten()
    });
    let matching = |run: &&VerificationRun| {
        crate::executor::goal_verification::run_matches_command(&run.label, command)
    };
    if let Some(run) = cited.filter(matching) {
        return classify(run, facts.mutation_ops);
    }
    if let Some(run) = facts.runs.iter().rev().find(matching) {
        return classify(run, facts.mutation_ops);
    }
    if let Some(started) = facts.pending.iter().rev().find(|started| {
        crate::executor::goal_verification::run_matches_command(&started.label, command)
    }) {
        return RunCheck::Pending(started);
    }
    RunCheck::Missing
}

fn classify(run: &VerificationRun, mutation_ops: u64) -> RunCheck<'_> {
    if !run.ok {
        return RunCheck::Failed(run);
    }
    if run.mutation_ops < mutation_ops {
        return RunCheck::Stale(run);
    }
    RunCheck::Found(run)
}

fn refusal_message(refusals: &[String]) -> String {
    let mut message = String::from(
        "update_goal(complete) was refused: the recorded requirement set is not fully accounted \
         for.\n",
    );
    for refusal in refusals.iter().take(8) {
        message.push_str("- ");
        message.push_str(refusal);
        message.push('\n');
    }
    if refusals.len() > 8 {
        message.push_str(&format!("- …and {} more\n", refusals.len() - 8));
    }
    message.push_str(
        "The goal remains active. Finish these requirements and call \
         update_goal(status=\"complete\", requirements=[{id, status: done|not_done|unverifiable, \
         evidence}]) with one entry per active deliverable id, or call \
         update_goal(status=\"blocked\", ...) with the exact obstruction. Do not narrow the \
         user's scope to what you already did.",
    );
    message
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_lifecycle::RequirementState;

    fn item(id: &str, kind: RequirementKind, text: &str, target: Option<&str>) -> RequirementItem {
        RequirementItem {
            id: id.to_string(),
            kind,
            text: text.to_string(),
            target: target.map(str::to_string),
            source_quote: text.to_string(),
            check: None,
            state: RequirementState::Active,
            superseded_by: None,
        }
    }

    fn run(label: &str, ok: bool, mutation_ops: u64, call_id: &str) -> VerificationRun {
        VerificationRun {
            label: label.to_string(),
            ok,
            exit_code: Some(if ok { 0 } else { 1 }),
            model_step: 1,
            mutation_ops,
            call_id: Some(call_id.to_string()),
        }
    }

    fn account(id: &str, status: AccountStatus, evidence: &str) -> RequirementAccount {
        RequirementAccount {
            id: id.to_string(),
            status,
            evidence: evidence.to_string(),
        }
    }

    fn accept_with(
        requirements: &[RequirementItem],
        accounts: &[RequirementAccount],
        runs: &[VerificationRun],
        mutation_ops: u64,
        artifact: &dyn Fn(&str) -> ArtifactState,
        literal: &dyn Fn(&str, &str) -> LiteralState,
    ) -> ScopeDecision {
        decide(&ScopeFacts {
            requirements,
            accounting: Some(accounts),
            runs,
            pending: &[],
            mutation_ops,
            artifact,
            literal,
        })
    }

    fn absent(_: &str) -> ArtifactState {
        ArtifactState::Absent
    }

    fn accept_verified(decision: ScopeDecision) -> Vec<VerificationRecord> {
        match decision {
            ScopeDecision::Accept {
                status: Some(VerificationStatus::Verified),
                warnings,
                records,
            } => {
                assert!(warnings.is_empty(), "{warnings:?}");
                records
            }
            other => panic!("expected a verified acceptance, got {other:?}"),
        }
    }

    fn accept_unverified(decision: ScopeDecision) -> Vec<VerificationRecord> {
        match decision {
            ScopeDecision::Accept {
                status: Some(VerificationStatus::Unverified),
                records,
                ..
            } => records,
            other => panic!("expected an unverified acceptance, got {other:?}"),
        }
    }

    fn unknown_literal(_: &str, _: &str) -> LiteralState {
        LiteralState::Unknown
    }

    #[test]
    fn every_active_deliverable_needs_an_account_keyed_by_id() {
        let requirements = [
            item("R1", RequirementKind::Named, "docs", None),
            item("R2", RequirementKind::Named, "benchmark", None),
        ];
        let accounts = [account("R1", AccountStatus::Done, "wrote docs/README.md")];
        let decision = accept_with(&requirements, &accounts, &[], 0, &absent, &unknown_literal);
        let ScopeDecision::Refuse(message) = decision else {
            panic!("an unaccounted requirement must refuse completion");
        };
        assert!(message.contains("R2"), "{message}");
        assert!(!message.contains("R1`"), "{message}");
        assert!(message.contains("no account"), "{message}");
    }

    #[test]
    fn a_not_done_account_refuses_completion() {
        let requirements = [item("R1", RequirementKind::Named, "docs", None)];
        let accounts = [account("R1", AccountStatus::NotDone, "not written yet")];
        let decision = accept_with(&requirements, &accounts, &[], 0, &absent, &unknown_literal);
        assert!(matches!(decision, ScopeDecision::Refuse(message) if message.contains("not_done")));
    }

    #[test]
    fn an_existing_artifact_confirms_a_done_claim() {
        let requirements = [item(
            "R1",
            RequirementKind::Artifact,
            "report",
            Some("done.txt"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "created done.txt")];
        let present = |_: &str| ArtifactState::Present;
        let decision = accept_with(&requirements, &accounts, &[], 0, &present, &unknown_literal);
        let records = accept_verified(decision);
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn a_missing_artifact_contradicts_a_done_claim() {
        let requirements = [item(
            "R1",
            RequirementKind::Artifact,
            "report",
            Some("done.txt"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "created done.txt")];
        let decision = accept_with(&requirements, &accounts, &[], 0, &absent, &unknown_literal);
        let ScopeDecision::Refuse(message) = decision else {
            panic!("a missing artifact must refuse completion");
        };
        assert!(message.contains("does not exist"), "{message}");
    }

    #[test]
    fn an_absent_artifact_must_really_be_gone() {
        let requirements = [item(
            "R1",
            RequirementKind::ArtifactAbsent,
            "old config",
            Some("old.toml"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "deleted old.toml")];
        let present = |_: &str| ArtifactState::Present;
        let decision = accept_with(&requirements, &accounts, &[], 0, &present, &unknown_literal);
        assert!(
            matches!(decision, ScopeDecision::Refuse(message) if message.contains("still exists"))
        );
    }

    #[test]
    fn a_content_literal_is_checked_against_its_target() {
        let requirements = [item(
            "R1",
            RequirementKind::Content,
            "Verified.",
            Some("README.md"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "appended the line")];
        let present = |_: &str, _: &str| LiteralState::Present;
        let decision = accept_with(&requirements, &accounts, &[], 0, &absent, &present);
        let records = accept_verified(decision);
        assert_eq!(records.len(), 1);

        let missing = |_: &str, _: &str| LiteralState::Absent;
        let decision = accept_with(&requirements, &accounts, &[], 0, &absent, &missing);
        assert!(
            matches!(decision, ScopeDecision::Refuse(message) if message.contains("does not appear"))
        );
    }

    #[test]
    fn a_command_requirement_needs_a_current_successful_run() {
        let requirements = [item(
            "R1",
            RequirementKind::Command,
            "run the verifier",
            Some("sh verify.sh"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "sh verify.sh")];
        let passed = [run("sh verify.sh", true, 3, "call-1")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &passed,
            3,
            &absent,
            &unknown_literal,
        );
        let records = accept_verified(decision);
        assert_eq!(records.len(), 1);

        let decision = accept_with(&requirements, &accounts, &[], 3, &absent, &unknown_literal);
        assert!(
            matches!(decision, ScopeDecision::Refuse(message) if message.contains("no successful run"))
        );

        let stale = [run("sh verify.sh", true, 2, "call-1")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &stale,
            3,
            &absent,
            &unknown_literal,
        );
        assert!(
            matches!(decision, ScopeDecision::Refuse(message) if message.contains("before the most recent change"))
        );
    }

    /// A command that only started has no result yet. Accounting for it as
    /// done is refused for the real missing fact, and the refusal names the
    /// background task to wait on.
    #[test]
    fn a_started_command_is_not_a_satisfied_command_requirement() {
        let requirements = [item(
            "R1",
            RequirementKind::Command,
            "run the verifier",
            Some("sh verify.sh"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "sh verify.sh")];
        let pending = [PendingCommand {
            label: "sh verify.sh".to_string(),
            task_id: Some("bg-1".to_string()),
            call_id: Some("call-1".to_string()),
        }];
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&accounts),
            runs: &[],
            pending: &pending,
            mutation_ops: 0,
            artifact: &absent,
            literal: &unknown_literal,
        });
        let ScopeDecision::Refuse(message) = decision else {
            panic!("a start is not a pass");
        };
        assert!(message.contains("only STARTED"), "{message}");
        assert!(message.contains("bg-1"), "{message}");
        assert!(message.contains("wait_task"), "{message}");
    }

    /// The record is the boundary between the declaration and the evidence:
    /// it names the exact execution, its exit status, and the workspace
    /// version it judged.
    #[test]
    fn an_accepted_record_carries_the_execution_evidence() {
        let requirements = [item(
            "R1",
            RequirementKind::Command,
            "the verifier must pass",
            Some("make regression"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "call_id:call-9")];
        let passed = [run("make regression", true, 4, "call-9")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &passed,
            4,
            &absent,
            &unknown_literal,
        );
        let records = accept_verified(decision);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].requirement_id, "R1");
        assert_eq!(records[0].kind, "command");
        assert_eq!(records[0].call_id.as_deref(), Some("call-9"));
        assert_eq!(records[0].command.as_deref(), Some("make regression"));
        assert_eq!(records[0].exit_code, Some(0));
        assert_eq!(records[0].workspace_version, 4);
        assert_eq!(records[0].model_step, 1);
    }

    /// One requirement resting on the model's account is enough to make the
    /// aggregate unverified, and the verified record keeps its own state.
    #[test]
    fn one_unverifiable_requirement_makes_the_aggregate_unverified() {
        let requirements = [
            item("R1", RequirementKind::Artifact, "report", Some("done.txt")),
            item("R2", RequirementKind::Named, "nice messages", None),
        ];
        let accounts = [
            account("R1", AccountStatus::Done, "created done.txt"),
            account("R2", AccountStatus::Done, "rewrote the message"),
        ];
        let present = |_: &str| ArtifactState::Present;
        let decision = accept_with(&requirements, &accounts, &[], 0, &present, &unknown_literal);
        match decision {
            ScopeDecision::Accept {
                status: Some(VerificationStatus::Unverified),
                records,
                ..
            } => {
                assert_eq!(records.len(), 2);
                assert_eq!(records[0].state, VerificationRecordState::Verified);
                assert_eq!(records[1].state, VerificationRecordState::Unverified);
            }
            other => panic!("one unverified boundary makes the whole unverified: {other:?}"),
        }
    }

    /// A passing command proves that the command exited 0. It does not prove
    /// that a test ran, or that any test result exists: the runtime has no
    /// structured test-result channel. A `tests` requirement therefore reaches
    /// the explicit unverified boundary even with its declared command's run,
    /// and the record keeps the execution evidence.
    #[test]
    fn a_declared_test_command_that_passed_is_unverified_with_its_evidence() {
        let requirements = [item(
            "R1",
            RequirementKind::Tests,
            "the suite must pass",
            Some("make regression"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "call_id:call-9")];
        let passed = [run("make regression", true, 4, "call-9")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &passed,
            4,
            &absent,
            &unknown_literal,
        );
        let records = accept_unverified(decision);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.kind, "tests");
        assert_eq!(record.state, VerificationRecordState::Unverified);
        assert_eq!(record.call_id.as_deref(), Some("call-9"));
        assert_eq!(record.command.as_deref(), Some("make regression"));
        assert_eq!(record.exit_code, Some(0));
        assert_eq!(record.workspace_version, 4);
        assert!(
            record.detail.contains("no structured test result"),
            "{}",
            record.detail
        );
    }

    /// A `true` or `echo` command is not a test suite. Declaring one as the
    /// test command and running it still cannot yield verified coverage.
    #[test]
    fn a_test_command_that_ran_no_tests_cannot_claim_verified_coverage() {
        let requirements = [item(
            "R1",
            RequirementKind::Tests,
            "the suite must pass",
            Some("true"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "call_id:call-7")];
        let passed = [run("true", true, 1, "call-7")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &passed,
            1,
            &absent,
            &unknown_literal,
        );
        let records = accept_unverified(decision);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, VerificationRecordState::Unverified);
    }

    /// The declared test command defines the evidence. Another command's
    /// success — including `cat` or `echo` — cannot satisfy it.
    #[test]
    fn an_unrelated_passing_command_cannot_satisfy_a_tests_requirement() {
        let requirements = [item(
            "R1",
            RequirementKind::Tests,
            "the suite must pass",
            Some("make regression"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "echo ok")];
        let passed = [run("echo ok", true, 1, "call-2")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &passed,
            1,
            &absent,
            &unknown_literal,
        );
        let ScopeDecision::Refuse(message) = decision else {
            panic!("an unrelated command satisfied a tests requirement");
        };
        assert!(message.contains("make regression"), "{message}");
    }

    /// Custom test commands remain first-class: the declared command is
    /// matched exactly, with no runner-name vocabulary.
    #[test]
    fn a_declared_custom_test_command_is_matched_by_its_declaration() {
        let requirements = [item(
            "R1",
            RequirementKind::Tests,
            "the project's own check must pass",
            Some("project-own-check --all"),
        )];
        let accounts = [account(
            "R1",
            AccountStatus::Done,
            "project-own-check --all",
        )];
        let passed = [run("project-own-check --all", true, 1, "call-3")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &passed,
            1,
            &absent,
            &unknown_literal,
        );
        let records = accept_unverified(decision);
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].command.as_deref(),
            Some("project-own-check --all")
        );
    }

    #[test]
    fn a_tests_requirement_without_a_declared_run_refuses() {
        let requirements = [item(
            "R1",
            RequirementKind::Tests,
            "the suite must pass",
            Some("make regression"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "I ran the tests")];
        let decision = accept_with(&requirements, &accounts, &[], 1, &absent, &unknown_literal);
        let ScopeDecision::Refuse(message) = decision else {
            panic!("a tests requirement was satisfied without a run");
        };
        assert!(message.contains("make regression"), "{message}");
        assert!(message.contains("no passing run"), "{message}");
    }

    #[test]
    fn a_failed_declared_test_command_refuses_a_done_claim() {
        let requirements = [item(
            "R1",
            RequirementKind::Tests,
            "the suite must pass",
            Some("make regression"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "call_id:call-3")];
        let failed = [run("make regression", false, 1, "call-3")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &failed,
            1,
            &absent,
            &unknown_literal,
        );
        assert!(matches!(decision, ScopeDecision::Refuse(message) if message.contains("failed")));
    }

    #[test]
    fn a_stale_declared_test_command_refuses_a_done_claim() {
        let requirements = [item(
            "R1",
            RequirementKind::Tests,
            "the suite must pass",
            Some("make regression"),
        )];
        let accounts = [account("R1", AccountStatus::Done, "call_id:call-3")];
        let passed = [run("make regression", true, 1, "call-3")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &passed,
            2,
            &absent,
            &unknown_literal,
        );
        assert!(
            matches!(decision, ScopeDecision::Refuse(message) if message.contains("before the most recent change"))
        );
    }

    /// Requirements persisted before `tests` required a `target` cannot be
    /// matched to a run. They remain finishable at the explicit unverified
    /// boundary instead of blocking a goal that was legally recorded.
    #[test]
    fn a_legacy_tests_requirement_without_a_target_is_unverified() {
        let requirements = [item(
            "R1",
            RequirementKind::Tests,
            "the suite must pass",
            None,
        )];
        let accounts = [account("R1", AccountStatus::Done, "call_id:call-9")];
        let passed = [run("make regression", true, 1, "call-9")];
        let decision = accept_with(
            &requirements,
            &accounts,
            &passed,
            1,
            &absent,
            &unknown_literal,
        );
        let records = accept_unverified(decision);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, VerificationRecordState::Unverified);
        assert!(
            records[0]
                .detail
                .contains("without a declared test command"),
            "{}",
            records[0].detail
        );
    }

    #[test]
    fn a_named_deliverable_is_accepted_as_an_unverified_boundary() {
        let requirements = [item(
            "R1",
            RequirementKind::Named,
            "nice error messages",
            None,
        )];
        let accounts = [account("R1", AccountStatus::Done, "rewrote the message")];
        let decision = accept_with(&requirements, &accounts, &[], 0, &absent, &unknown_literal);
        match decision {
            ScopeDecision::Accept {
                status: Some(VerificationStatus::Unverified),
                warnings,
                records,
            } => {
                assert!(
                    warnings.len() == 1 && warnings[0].contains("no mechanical check"),
                    "{warnings:?}"
                );
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].requirement_id, "R1");
                assert_eq!(records[0].state, VerificationRecordState::Unverified);
            }
            other => panic!("a named deliverable is accepted as unverified: {other:?}"),
        }
    }

    #[test]
    fn constraints_are_not_owed_and_need_no_account() {
        let requirements = [item(
            "R1",
            RequirementKind::Constraint,
            "keep the public API stable",
            None,
        )];
        let decision = accept_with(&requirements, &[], &[], 0, &absent, &unknown_literal);
        assert_eq!(
            decision,
            ScopeDecision::Accept {
                status: None,
                warnings: Vec::new(),
                records: Vec::new(),
            }
        );
    }

    #[test]
    fn a_waived_requirement_is_not_owed() {
        let mut waived = item("R1", RequirementKind::Named, "docs", None);
        waived.state = RequirementState::Waived;
        let decision = accept_with(&[waived], &[], &[], 0, &absent, &unknown_literal);
        assert_eq!(
            decision,
            ScopeDecision::Accept {
                status: None,
                warnings: Vec::new(),
                records: Vec::new(),
            }
        );
    }

    #[test]
    fn duplicate_accounts_for_one_id_are_refused() {
        let requirements = [item("R1", RequirementKind::Named, "docs", None)];
        let accounts = [
            account("R1", AccountStatus::Done, "first"),
            account("R1", AccountStatus::Done, "second"),
        ];
        let decision = accept_with(&requirements, &accounts, &[], 0, &absent, &unknown_literal);
        assert!(
            matches!(decision, ScopeDecision::Refuse(message) if message.contains("more than once"))
        );
    }

    #[test]
    fn the_accounting_shape_is_validated() {
        let arguments = serde_json::json!({"requirements": [{"id": "R1", "status": "done"}]});
        assert!(parse_accounting(&arguments, 1).is_err());
        let arguments =
            serde_json::json!({"requirements": [{"id": "", "status": "done", "evidence": "x"}]});
        assert!(parse_accounting(&arguments, 1).is_err());
        let arguments = serde_json::json!({"requirements": [
            {"id": "R1", "status": "done", "evidence": "created it"}
        ]});
        assert_eq!(
            parse_accounting(&arguments, 1)
                .expect("parses")
                .expect("present")
                .len(),
            1
        );
        let arguments = serde_json::json!({"requirements": (0..21)
            .map(|index| serde_json::json!({
                "id": format!("R{index}"),
                "status": "done",
                "evidence": "done",
            }))
            .collect::<Vec<_>>()});
        assert!(parse_accounting(&arguments, 1).is_err());
    }
}
