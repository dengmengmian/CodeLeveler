//! Parallel review (spec §44): several reviewers examine the diff concurrently,
//! each with a distinct responsibility (correctness / security / tests), and a
//! merger deduplicates and ranks the findings.

use serde::{Deserialize, Serialize};

/// A finding's severity, ordered so `Critical` is greatest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "critical" | "blocker" => Severity::Critical,
            "high" => Severity::High,
            "medium" | "moderate" => Severity::Medium,
            "low" => Severity::Low,
            _ => Severity::Info,
        }
    }
}

/// A single review finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub lens: String,
    pub severity: Severity,
    pub file: Option<String>,
    pub issue: String,
}

/// A reviewer lens that did not complete. Kept distinct from an empty finding
/// list so callers never mistake infrastructure failure for a clean review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFailure {
    pub lens: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReviewReport {
    pub lenses_run: usize,
    pub findings: Vec<ReviewFinding>,
    pub failures: Vec<ReviewFailure>,
}

/// A distinct reviewer responsibility (spec §44: no shared broad prompt).
#[derive(Debug, Clone, Copy)]
pub struct ReviewLens {
    pub name: &'static str,
    pub system: &'static str,
}

/// The default review panel. Each lens has a narrow, explicit charter.
pub const DEFAULT_LENSES: &[ReviewLens] = &[
    ReviewLens {
        name: "correctness",
        system: "You are a correctness reviewer. Examine ONLY whether the diff \
                 correctly and completely implements the stated goal: logic errors, \
                 missed cases, wrong conditions, unhandled errors, broken callers. \
                 Ignore style, security, and tests.",
    },
    ReviewLens {
        name: "security",
        system: "You are a security reviewer. Examine ONLY security concerns in the \
                 diff: injection, unsafe input handling, secret exposure, path/auth \
                 problems, unsafe defaults. Ignore correctness, style, and tests.",
    },
    ReviewLens {
        name: "tests",
        system: "You are a test reviewer. Examine ONLY whether the change is \
                 adequately tested and whether tests were weakened or made to pass \
                 without testing real behavior. Ignore unrelated concerns.",
    },
];

/// The JSON the reviewer model returns.
#[derive(Debug, Deserialize)]
pub struct RawReview {
    #[serde(default)]
    pub findings: Vec<RawFinding>,
}

#[derive(Debug, Deserialize)]
pub struct RawFinding {
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub issue: String,
}

/// The reviewer prompt shared across lenses (schema instructions).
pub fn review_user_prompt(goal: &str, diff: &str) -> String {
    format!(
        "Goal: {goal}\n\nReview this unified diff and report problems as JSON:\n\
         {{\"findings\": [{{\"severity\": one of \
         [\"info\",\"low\",\"medium\",\"high\",\"critical\"], \"file\": string, \
         \"issue\": string}}]}}.\n\n\
         What counts as a finding — a claim fails ANY of these, drop it:\n\
         - The author would actually fix it if they saw it. Trivial style is not a finding.\n\
         - It was introduced by THIS diff. Pre-existing problems are out of scope.\n\
         - It is discrete and actionable, not a general complaint about the codebase.\n\
         - It does not rest on assumptions about intent that the diff does not support; \
         an intentional change is not a bug.\n\
         - If you claim the change breaks something elsewhere, you must name the code \
         that provably breaks. Speculation that it 'might affect' other callers is not \
         a finding.\n\n\
         Report only problems within your charter. An empty findings list is the correct \
         answer when there is nothing worth fixing — do not manufacture a finding to look \
         thorough. Output ONLY the JSON.\n\nDiff:\n{diff}"
    )
}

/// Merge findings from all lenses: drop empties, dedup by (file, issue), sort by
/// severity (highest first).
pub fn merge_findings(mut findings: Vec<ReviewFinding>) -> Vec<ReviewFinding> {
    findings.retain(|f| !f.issue.trim().is_empty());
    let mut seen = std::collections::HashSet::new();
    findings.retain(|f| {
        let key = format!("{:?}|{}", f.file, f.issue.to_lowercase());
        seen.insert(key)
    });
    findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
    findings
}

/// The highest severity present, if any.
pub fn max_severity(findings: &[ReviewFinding]) -> Option<Severity> {
    findings.iter().map(|f| f.severity).max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(sev: Severity, issue: &str) -> ReviewFinding {
        ReviewFinding {
            lens: "t".into(),
            severity: sev,
            file: Some("x.rs".into()),
            issue: issue.into(),
        }
    }

    /// A lens charter alone ("you are a correctness reviewer") sets no bar for
    /// what survives as a finding, so the panel returns pre-existing issues,
    /// style nits, and "this might break other callers" speculation. The prompt
    /// must state the filters — and that finding nothing is a valid answer.
    #[test]
    fn review_prompt_states_what_disqualifies_a_finding() {
        let prompt = review_user_prompt("fix the parser", "--- a/x\n+++ b/x\n");

        assert!(
            prompt.contains("introduced by THIS diff"),
            "pre-existing problems are out of scope"
        );
        assert!(
            prompt.contains("would actually fix it"),
            "noise must be filtered by author intent"
        );
        assert!(
            prompt.contains("name the code that provably breaks"),
            "speculation about other callers is not a finding"
        );
        assert!(
            prompt.contains("do not manufacture a finding"),
            "an empty list must be an acceptable answer"
        );
    }

    #[test]
    fn severity_orders_and_parses() {
        assert!(Severity::Critical > Severity::High);
        assert_eq!(Severity::parse("BLOCKER"), Severity::Critical);
        assert_eq!(Severity::parse("nonsense"), Severity::Info);
    }

    #[test]
    fn merge_dedups_and_sorts() {
        let merged = merge_findings(vec![
            f(Severity::Low, "same"),
            f(Severity::Low, "same"),
            f(Severity::Critical, "boom"),
            f(Severity::Info, ""),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].severity, Severity::Critical);
    }

    #[test]
    fn max_severity_of_findings() {
        let findings = vec![f(Severity::Low, "a"), f(Severity::High, "b")];
        assert_eq!(max_severity(&findings), Some(Severity::High));
    }
}
