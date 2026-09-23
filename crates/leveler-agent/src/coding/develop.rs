//! The Develop workflow: `Analyze → Coding → Review`.
//!
//! An explicitly requested, complete development loop. It is Coding-Harness
//! state, not Runtime state: the engine keeps owning lifecycle, ownership and
//! persistence, and knows nothing about work orders, review verdicts or which
//! model reads the code.
//!
//! Nothing here runs unless the user asked for it by name. An ordinary turn
//! never reaches this module.

use leveler_model::ModelRef;

/// Which model the Develop workflow's *reading* stages run on.
///
/// Analyze and Review are the same model by design (one knob, not two). The
/// rule has exactly two inputs and no third state:
///
/// ```text
/// develop.model absent     → the session's own model (a second model is an
///                            enhancement, never a requirement)
/// develop.model configured → that model, or a hard error
/// ```
///
/// An explicitly configured model is never quietly replaced. A user who wrote
/// a model name down and got someone else's model would believe Analyze read
/// their code with a reasoning model when it did not — so a value that does
/// not parse is an error, not a fallback.
pub fn resolve_develop_model(
    configured: Option<&str>,
    default: &ModelRef,
) -> Result<ModelRef, String> {
    let Some(raw) = configured.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(default.clone());
    };
    ModelRef::parse(raw).ok_or_else(|| {
        format!(
            "develop.model `{raw}` is not a `provider/model` reference \
             (for example `deepseek/deepseek-v4-pro`)"
        )
    })
}

/// How many times Review may send the work back before the workflow stops.
///
/// The existing budgets do not cover this: `StepLimits` and
/// `ContinuationPolicy` bound a single turn, and `goals.windows_run` is
/// counted but never read as a ceiling. So the loop needs one bound of its
/// own — deliberately a constant and not a user-facing knob, because a user
/// who has to tune a retry count is being asked to manage a defect.
///
/// It counts REWORK and REANALYZE together. Two separate counters would let a
/// workflow alternate between them forever, which is the unbounded loop this
/// exists to prevent.
const MAX_REVIEW_SENDBACKS: u32 = 2;

/// Review's verdict on the delivered change. Three outcomes, no fourth.
///
/// There is deliberately no `PARTIAL_PASS`, no `ESCALATE` and no
/// `REVIEW_AGAIN`: each of those is a way of not deciding, and the workflow's
/// value is that somebody decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewDecision {
    /// The delivered change actually solves the user's original goal.
    Pass,
    /// The analysis was right; the implementation is not finished or not
    /// correct. Coding runs again under a new work order.
    Rework,
    /// The original analysis named the wrong root cause or the wrong
    /// boundary. Analyze runs again from the user's goal.
    Reanalyze,
}

/// What the workflow does next, once Review has spoken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevelopStep {
    /// Re-read the code and produce a new work order.
    Analyze,
    /// Hand the work order to the full Coding agent again.
    Coding,
    /// Stop, with a terminal the user can trust.
    Stop(DevelopTerminal),
}

/// How a Develop workflow ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevelopTerminal {
    /// Review read the actual change and accepted it.
    ReviewPassed,
    /// The loop hit its bound with Review still asking for changes. This is
    /// NOT a pass and must never be reported as one.
    StoppedBeforeReviewPassed { sendbacks: u32 },
    /// Review produced no verdict the runtime could act on — it ran out of
    /// rounds, failed to launch, or wrote something that was not a decision.
    /// Also not a pass: nobody accepted this change.
    ReviewDidNotDecide,
}

impl DevelopTerminal {
    /// Whether the workflow delivered what the user asked for.
    ///
    /// A run that stopped at its ceiling answers `false` here. Spending the
    /// budget is not the same as finishing the work, and the one thing the
    /// user must be able to rely on is that this distinction survives.
    pub fn review_passed(&self) -> bool {
        matches!(self, Self::ReviewPassed)
    }
}

/// The line Review is asked to end its report with.
const DECISION_MARKER: &str = "DECISION:";

/// Read Review's verdict out of its report.
///
/// `None` means the report did not state a verdict the runtime can act on.
/// The caller must treat that as "Review did not decide" and never as a pass:
/// a model that wandered off, ran out of rounds, or wrote prose about how the
/// change *would* pass has not accepted anything. Free prose is not a verdict,
/// which is why only an explicit marker line counts.
///
/// When a report carries several marker lines — a model that reasoned out loud
/// before committing — the LAST one is the verdict, because that is the one it
/// settled on.
pub fn parse_review_decision(report: &str) -> Option<ReviewDecision> {
    report.lines().rev().find_map(|line| {
        let line = line.trim();
        let (head, verdict) = line.split_once(':')?;
        if !head
            .trim()
            .eq_ignore_ascii_case(DECISION_MARKER.trim_end_matches(':'))
        {
            return None;
        }
        match verdict.trim().to_ascii_uppercase().as_str() {
            "PASS" => Some(ReviewDecision::Pass),
            "REWORK" => Some(ReviewDecision::Rework),
            "REANALYZE" => Some(ReviewDecision::Reanalyze),
            _ => None,
        }
    })
}

/// The task handed to the Analyze stage.
///
/// Analyze is not "make a plan": the Coding agent has a plan of its own and
/// keeps making it. Analyze is the senior engineer who reads the code first
/// and writes down what the next engineer must know — the root cause, the
/// boundary, what must not break, and what "done" means for THIS goal.
pub fn analyze_brief(goal: &str) -> String {
    format!(
        "You are the Analyze stage of a development workflow. Another engineer — a full \
         coding agent with its own plan, tools and judgement — will implement the change \
         after you. You cannot edit anything; you read.\n\
         \n\
         The user's goal, verbatim:\n\
         ---\n{goal}\n---\n\
         \n\
         Read the real code before you conclude anything: follow the actual call chain, \
         open the files that matter, and check what the current behaviour is. Do not \
         describe what the code probably does.\n\
         \n\
         Then write a work order for the implementing engineer. Cover, in your own words \
         and in whatever order serves the task:\n\
         - the goal, restated as you now understand it;\n\
         - what you established by reading, with file:line references;\n\
         - the root cause, and why it produces the reported behaviour;\n\
         - what should change, and where;\n\
         - what must NOT change: architectural boundaries, existing contracts, callers \
           that would break;\n\
         - how the result should be verified, and what evidence would show it works;\n\
         - what the implementer should report back.\n\
         \n\
         Be specific enough that the implementer does not have to re-derive your \
         investigation, and leave them the judgement calls that only show up while \
         writing the code. If the goal is ambiguous in a way that changes the work, say \
         so explicitly instead of picking one reading silently.\n\
         \n\
         Your entire reply is the work order. Do not address the user."
    )
}

/// The task handed to Analyze when Review rejected the previous analysis.
///
/// It gets the rejected work order and why it was rejected, because the whole
/// point of REANALYZE is that reading the code the same way again would reach
/// the same wrong conclusion.
pub fn reanalyze_brief(goal: &str, rejected: &str, review: &str) -> String {
    format!(
        "{}\n\
         \n\
         ---\n\
         This is a SECOND analysis. A previous work order was implemented and then \
         rejected by review, which found the analysis itself wrong — not merely the \
         implementation.\n\
         \n\
         The rejected work order:\n\
         ---\n{rejected}\n---\n\
         \n\
         Why review rejected it:\n\
         ---\n{review}\n---\n\
         \n\
         Do not restate the previous analysis with different wording. Read the code again \
         from the goal, and take review's objection seriously enough to look where it \
         points. If after reading you still believe the original root cause was right, say \
         so and explain what review misread — an analysis that caves to a wrong objection \
         is as useless as one that ignores a right one.",
        analyze_brief(goal)
    )
}

/// The task handed to the Coding agent: the work order, with the user's goal
/// kept verbatim above it.
///
/// The goal stays first and unedited on purpose. A work order is one engineer's
/// reading of the request, and an implementer who only ever sees the reading
/// cannot notice when the reading drifted from what was asked.
pub fn coding_task(goal: &str, work_order: &str) -> String {
    format!(
        "{goal}\n\
         \n\
         ---\n\
         The following work order was produced by an engineer who read the code for this \
         goal. Treat it as well-informed instructions, not as a script: you keep your own \
         judgement while implementing, and if the code contradicts the work order, the \
         code wins — say so in your report.\n\
         ---\n\
         \n\
         {work_order}"
    )
}

/// The task handed to the Review stage.
///
/// Review's question is not "is this good code". It is: does what was actually
/// delivered solve what the user actually asked for. That is why it gets the
/// original goal rather than only the work order — an implementation can
/// satisfy a work order that drifted.
pub fn review_brief(
    goal: &str,
    work_order: &str,
    coding_report: &str,
    modified_files: &[String],
    diff: Option<&str>,
) -> String {
    let files = if modified_files.is_empty() {
        "(none)".to_string()
    } else {
        modified_files.join("\n")
    };
    let diff = match diff {
        Some(diff) if !diff.trim().is_empty() => diff,
        _ => "(no diff available — read the files yourself)",
    };
    format!(
        "You are the Review stage of a development workflow. Decide whether what was \
         actually delivered solves the user's original goal. You cannot edit anything.\n\
         \n\
         The user's original goal, verbatim:\n\
         ---\n{goal}\n---\n\
         \n\
         The work order the implementer was given:\n\
         ---\n{work_order}\n---\n\
         \n\
         What the implementer reported:\n\
         ---\n{coding_report}\n---\n\
         \n\
         Files changed:\n\
         ---\n{files}\n---\n\
         \n\
         The change:\n\
         ---\n{diff}\n---\n\
         \n\
         The implementer's report is a claim, not evidence. Read the changed code \
         yourself before deciding, and read around it far enough to see what the change \
         affects.\n\
         \n\
         State your findings, each with file:line and why it matters. Then end your reply \
         with exactly one line:\n\
         \n\
         {DECISION_MARKER} PASS       — the delivered change solves the original goal and \
         you found nothing substantive left to fix.\n\
         {DECISION_MARKER} REWORK     — the analysis was right, the implementation is not \
         finished or not correct.\n\
         {DECISION_MARKER} REANALYZE  — the work order itself named the wrong root cause \
         or the wrong boundary.\n\
         \n\
         For REWORK or REANALYZE, everything above that line must be a complete brief for \
         whoever picks the work up: what is wrong, where, and what has to be true for it \
         to be right. They will not see this conversation.\n\
         \n\
         Only those three are verdicts. If you cannot reach one, say why, and do not \
         write a decision line at all — an absent verdict is handled honestly, a guessed \
         one is not."
    )
}

/// The send-backs spent so far. The workflow's only mutable state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DevelopProgress {
    sendbacks: u32,
}

impl DevelopProgress {
    /// Apply Review's verdict and say what happens next.
    pub fn advance(&mut self, decision: Option<ReviewDecision>) -> DevelopStep {
        let Some(decision) = decision else {
            return DevelopStep::Stop(DevelopTerminal::ReviewDidNotDecide);
        };
        match decision {
            ReviewDecision::Pass => DevelopStep::Stop(DevelopTerminal::ReviewPassed),
            ReviewDecision::Rework | ReviewDecision::Reanalyze => {
                if self.sendbacks >= MAX_REVIEW_SENDBACKS {
                    return DevelopStep::Stop(DevelopTerminal::StoppedBeforeReviewPassed {
                        sendbacks: self.sendbacks,
                    });
                }
                self.sendbacks += 1;
                match decision {
                    ReviewDecision::Reanalyze => DevelopStep::Analyze,
                    _ => DevelopStep::Coding,
                }
            }
        }
    }

    /// How many times Review has sent the work back.
    pub fn sendbacks(&self) -> u32 {
        self.sendbacks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_model() -> ModelRef {
        ModelRef::new("deepseek", "deepseek-v4-flash")
    }

    #[test]
    fn an_explicit_verdict_line_is_read() {
        for (report, expected) in [
            ("DECISION: PASS", ReviewDecision::Pass),
            ("DECISION: REWORK", ReviewDecision::Rework),
            ("DECISION: REANALYZE", ReviewDecision::Reanalyze),
            ("decision: pass", ReviewDecision::Pass),
            ("   DECISION:   REWORK   ", ReviewDecision::Rework),
        ] {
            assert_eq!(
                parse_review_decision(report),
                Some(expected),
                "failed to read `{report}`"
            );
        }
    }

    #[test]
    fn the_verdict_is_read_from_a_full_report() {
        let report = "\
I read crates/leveler-tui/src/plan_cell.rs and the change does update the
plan rows, but it never redraws when the step count shrinks.

Findings:
- plan_cell.rs:88 keeps a stale row.

DECISION: REWORK
";
        assert_eq!(parse_review_decision(report), Some(ReviewDecision::Rework));
    }

    #[test]
    fn prose_about_passing_is_not_a_verdict() {
        for report in [
            "",
            "   \n  \n",
            "This change looks good and the tests would pass.",
            "I think we should probably rework the approach, but I ran out of time.",
            "The verification report says passed.",
        ] {
            assert_eq!(
                parse_review_decision(report),
                None,
                "free prose must not be read as a verdict: {report:?}"
            );
        }
    }

    #[test]
    fn an_unreadable_verdict_is_never_a_pass() {
        for report in ["DECISION: MAYBE", "DECISION:", "DECISION: PARTIAL_PASS"] {
            assert_ne!(
                parse_review_decision(report),
                Some(ReviewDecision::Pass),
                "an unrecognised verdict must never be promoted to PASS: {report:?}"
            );
        }
    }

    #[test]
    fn the_last_verdict_is_the_one_it_settled_on() {
        let report = "\
My first instinct was DECISION: REANALYZE.
Then I read the call chain again and the root cause was right after all.

DECISION: REWORK
";
        assert_eq!(
            parse_review_decision(report),
            Some(ReviewDecision::Rework),
            "a model that reasons out loud settles on its final line"
        );
    }

    #[test]
    fn the_coding_task_keeps_the_user_goal_verbatim_above_the_work_order() {
        let task = coding_task("修复计划进度不更新", "Root cause: plan_cell.rs:88 …");
        assert!(
            task.starts_with("修复计划进度不更新"),
            "the implementer must see what was asked, not only how it was read: {task}"
        );
        assert!(task.contains("Root cause: plan_cell.rs:88 …"));
    }

    #[test]
    fn the_review_brief_asks_for_a_verdict_this_runtime_can_read() {
        let brief = review_brief(
            "修复计划进度不更新",
            "work order",
            "I changed plan_cell.rs",
            &["crates/leveler-tui/src/plan_cell.rs".to_string()],
            Some("@@ -88 +88 @@"),
        );
        // The three verdicts the parser accepts must be exactly the three the
        // brief offers, or the workflow asks for answers it cannot read.
        for verdict in ["PASS", "REWORK", "REANALYZE"] {
            let line = format!("{DECISION_MARKER} {verdict}");
            assert!(brief.contains(&line), "brief must offer `{line}`");
            assert_eq!(
                parse_review_decision(&line),
                Some(match verdict {
                    "PASS" => ReviewDecision::Pass,
                    "REWORK" => ReviewDecision::Rework,
                    _ => ReviewDecision::Reanalyze,
                })
            );
        }
        assert!(
            brief.contains("修复计划进度不更新"),
            "Review judges against the ORIGINAL goal, not the work order"
        );
        assert!(brief.contains("@@ -88 +88 @@"), "the diff must reach it");
    }

    #[test]
    fn the_review_brief_says_so_when_there_is_no_diff() {
        let brief = review_brief("g", "w", "r", &[], None);
        assert!(brief.contains("(none)"), "an empty file list is stated");
        assert!(
            brief.contains("no diff available"),
            "a missing diff is named, never silently omitted"
        );
    }

    #[test]
    fn a_review_that_did_not_decide_stops_the_workflow_without_passing() {
        // A reviewer that ran out of rounds, or wrote prose instead of a
        // verdict, has not accepted the change. Spending another Coding round
        // on a send-back nobody asked for would be inventing a verdict; so
        // would passing. The workflow stops and says which happened.
        let mut progress = DevelopProgress::default();
        let step = progress.advance(None);
        let DevelopStep::Stop(terminal) = step else {
            panic!("an undecided review must stop the workflow: {step:?}");
        };
        assert_eq!(terminal, DevelopTerminal::ReviewDidNotDecide);
        assert!(!terminal.review_passed());
        assert_eq!(
            progress.sendbacks(),
            0,
            "a missing verdict is not a send-back"
        );
    }

    #[test]
    fn a_pass_stops_the_workflow_as_passed() {
        let mut progress = DevelopProgress::default();
        assert_eq!(
            progress.advance(Some(ReviewDecision::Pass)),
            DevelopStep::Stop(DevelopTerminal::ReviewPassed)
        );
        assert_eq!(progress.sendbacks(), 0);
    }

    #[test]
    fn a_rework_runs_coding_again_without_re_analyzing() {
        let mut progress = DevelopProgress::default();
        assert_eq!(
            progress.advance(Some(ReviewDecision::Rework)),
            DevelopStep::Coding
        );
        assert_eq!(progress.sendbacks(), 1);
    }

    #[test]
    fn a_reanalyze_goes_back_to_analyze() {
        let mut progress = DevelopProgress::default();
        assert_eq!(
            progress.advance(Some(ReviewDecision::Reanalyze)),
            DevelopStep::Analyze
        );
        assert_eq!(progress.sendbacks(), 1);
    }

    #[test]
    fn the_loop_is_bounded() {
        let mut progress = DevelopProgress::default();
        for _ in 0..MAX_REVIEW_SENDBACKS {
            assert_eq!(
                progress.advance(Some(ReviewDecision::Rework)),
                DevelopStep::Coding
            );
        }
        assert_eq!(
            progress.advance(Some(ReviewDecision::Rework)),
            DevelopStep::Stop(DevelopTerminal::StoppedBeforeReviewPassed {
                sendbacks: MAX_REVIEW_SENDBACKS
            }),
            "a workflow that keeps failing review must stop on its own"
        );
    }

    #[test]
    fn alternating_rework_and_reanalyze_cannot_outrun_the_bound() {
        let mut progress = DevelopProgress::default();
        let mut steps = Vec::new();
        for turn in 0..10 {
            let decision = if turn % 2 == 0 {
                ReviewDecision::Rework
            } else {
                ReviewDecision::Reanalyze
            };
            let step = progress.advance(Some(decision));
            let stopped = matches!(step, DevelopStep::Stop(_));
            steps.push(step);
            if stopped {
                break;
            }
        }
        assert_eq!(
            steps.len() as u32,
            MAX_REVIEW_SENDBACKS + 1,
            "two counters would let the workflow alternate forever: {steps:?}"
        );
    }

    #[test]
    fn hitting_the_ceiling_is_never_reported_as_a_pass() {
        let mut progress = DevelopProgress::default();
        let mut last = DevelopStep::Coding;
        for _ in 0..MAX_REVIEW_SENDBACKS + 1 {
            last = progress.advance(Some(ReviewDecision::Rework));
        }
        let DevelopStep::Stop(terminal) = last else {
            panic!("the bounded loop must stop: {last:?}");
        };
        assert!(
            !terminal.review_passed(),
            "reaching the retry ceiling is not a pass"
        );
    }

    #[test]
    fn an_absent_develop_model_inherits_the_session_model() {
        let resolved = resolve_develop_model(None, &default_model()).expect("absent is legal");
        assert_eq!(
            resolved,
            default_model(),
            "`/develop` must work with no develop.model configured at all"
        );
    }

    #[test]
    fn a_blank_develop_model_is_not_a_configuration() {
        for blank in ["", "   "] {
            let resolved =
                resolve_develop_model(Some(blank), &default_model()).expect("blank is not a value");
            assert_eq!(resolved, default_model());
        }
    }

    #[test]
    fn a_configured_develop_model_is_used() {
        let resolved = resolve_develop_model(Some("openai/gpt-5.6"), &default_model())
            .expect("a well-formed reference resolves");
        assert_eq!(resolved, ModelRef::new("openai", "gpt-5.6"));
    }

    #[test]
    fn an_explicitly_invalid_develop_model_is_an_error_not_a_fallback() {
        let error = resolve_develop_model(Some("gpt-5.6"), &default_model())
            .expect_err("a bare model name is not a `provider/model` reference");
        assert!(
            error.contains("gpt-5.6"),
            "the refusal must name the value the user wrote: {error}"
        );
        assert!(
            error.contains("provider/model"),
            "the refusal must say what shape was expected: {error}"
        );
    }
}
