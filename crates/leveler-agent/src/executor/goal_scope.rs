//! Acceptance-scope integrity for Goal completion.
//!
//! The user's request — not the model's acceptance list — defines what the
//! task owes. The runtime cannot read arbitrary prose, so it does only the
//! narrow thing it can do deterministically:
//!
//! - It extracts requirements the user *explicitly enumerated*: file paths,
//!   inline-code tokens, short delimiter-separated lists, numbered items, and
//!   an explicit demand that tests pass. Open-ended prose yields no
//!   requirements; it is never guessed at.
//! - At `commit_task`, every extracted requirement must be covered by the
//!   model's acceptance checks.
//! - At `update_goal(complete)`, every requirement must be accounted for, and
//!   the claims the runtime *can* check mechanically (artifact existence, a
//!   named command running, a test-class command passing) are checked against
//!   the execution record.
//!
//! A named requirement with no mechanical check can still complete, but only
//! as an explicitly unverified boundary: the runtime never reports a semantic
//! claim as mechanically verified.

use crate::executor::goal_verification::VerificationRun;

/// What kind of evidence can decide one requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequirementKind {
    /// A file/directory the user named. `removed` inverts the existence check.
    Artifact { removed: bool },
    /// An executable path the user asked to run (`sh ./verify.sh`).
    Command,
    /// An explicitly enumerated deliverable with no mechanical check.
    Named,
    /// The user explicitly required the test suite to pass.
    Tests,
}

/// Where the requirement came from. The text is the user's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequirementSource {
    Objective,
    Amendment(String),
}

/// One explicit requirement extracted from user-authored text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Requirement {
    /// Stable diagnostic identity (`R1`, `R2`, ...) derived from the durable
    /// objective plus amendments, so it survives resume and compaction.
    pub id: String,
    /// The token exactly as the user wrote it (trimmed).
    pub text: String,
    pub kind: RequirementKind,
    pub source: RequirementSource,
}

/// The model's per-requirement account in `update_goal(complete)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequirementAccount {
    pub requirement: String,
    pub status: AccountStatus,
    pub evidence: String,
}

/// `done` is a delivery claim, `not_done` is an admission, and `unverifiable`
/// says the work was delivered but the runtime cannot prove it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountStatus {
    Done,
    NotDone,
    Unverifiable,
}

/// What the runtime could observe about one artifact path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArtifactState {
    Present,
    Absent,
    /// No workspace, an absolute path outside it, or a path that escapes it.
    Unknown,
}

/// Everything [`decide`] needs. Pure data so the gate is unit-testable.
pub(crate) struct ScopeFacts<'a> {
    pub requirements: &'a [Requirement],
    pub accounting: Option<&'a [RequirementAccount]>,
    pub runs: &'a [VerificationRun],
    pub mutation_ops: u64,
    pub artifact: &'a dyn Fn(&str) -> ArtifactState,
}

/// The scope gate's answer for one `update_goal(complete)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopeDecision {
    /// Completion may proceed. Warnings name every boundary the runtime could
    /// not verify mechanically.
    Accept { warnings: Vec<String> },
    /// Completion is refused; the goal stays active and the model sees exactly
    /// which user requirements it did not account for.
    Refuse(String),
}

/// File extensions treated as a named artifact. A token with a slash is a
/// path regardless of extension.
const PATH_EXTENSIONS: &[&str] = &[
    "rs",
    "go",
    "py",
    "pyi",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "tsx",
    "vue",
    "svelte",
    "json",
    "jsonl",
    "yaml",
    "yml",
    "toml",
    "md",
    "txt",
    "sh",
    "bash",
    "zsh",
    "ps1",
    "bat",
    "cmd",
    "sql",
    "css",
    "html",
    "htm",
    "xml",
    "ini",
    "conf",
    "cfg",
    "c",
    "h",
    "cc",
    "cpp",
    "hpp",
    "java",
    "kt",
    "kts",
    "swift",
    "rb",
    "php",
    "cs",
    "fs",
    "ex",
    "exs",
    "lua",
    "r",
    "scala",
    "dart",
    "gradle",
    "lock",
    "env",
    "proto",
    "graphql",
    "tf",
    "dockerfile",
];

/// Characters that end a path-like token. Commas and ideographic commas end a
/// token; a dot does not, because it belongs to the extension.
const TOKEN_BREAKS: &[char] = &[
    ' ', '\t', '\r', '、', '，', ',', '；', ';', '。', '：', ':', '!', '?', '！', '？', '(', ')',
    '（', '）', '[', ']', '【', '】', '{', '}', '<', '>', '"', '\'', '“', '”', '‘', '’', '`',
];

/// Sentence-level boundaries. Commas stay inside because they are list
/// separators in an enumeration.
const CLAUSE_BREAKS: &[char] = &['\n', '。', '；', ';'];

/// Leading verbs stripped from an enumerated item so the requirement is the
/// deliverable, not the instruction.
const LEADING_VERBS: &[&str] = &[
    "完成",
    "实现",
    "创建",
    "新增",
    "添加",
    "修改",
    "更新",
    "支持",
    "接入",
    "交付",
    "构建",
    "编写",
    "补齐",
    "补充",
    "修复",
    "删除",
    "移除",
    "重命名",
    "迁移",
    "implement",
    "create",
    "add",
    "build",
    "write",
    "update",
    "fix",
    "support",
    "deliver",
    "complete",
    "finish",
    "migrate",
    "rename",
    "remove",
    "delete",
];

const RUN_VERBS: &[&str] = &["运行", "执行", "跑", "调用", "run", "execute", "invoke"];

const NEGATIONS: &[&str] = &[
    "不要",
    "禁止",
    "不得",
    "不允许",
    "不需要",
    "无需",
    "别",
    "do not",
    "don't",
    "never",
    "must not",
];

const MUTATION_VERBS: &[&str] = &[
    "修改", "改动", "编辑", "触碰", "删除", "移除", "change", "edit", "modify", "touch", "delete",
    "remove",
];

const REMOVAL_VERBS: &[&str] = &["删除", "移除", "删掉", "清理掉", "delete", "remove", "rm "];

const DELIVERY_VERBS: &[&str] = &[
    "实现",
    "完成",
    "创建",
    "新增",
    "添加",
    "修改",
    "更新",
    "支持",
    "接入",
    "交付",
    "构建",
    "编写",
    "补齐",
    "补充",
    "修复",
    "implement",
    "create",
    "add",
    "build",
    "write",
    "update",
    "fix",
    "support",
    "deliver",
    "complete",
    "finish",
    "migrate",
    "rename",
];

/// Waiver markers. Deliberately narrow: a waiver must be an explicit
/// deferral/skip, not a general negative sentence.
const WAIVER_MARKERS: &[&str] = &[
    "暂时不做",
    "先不做",
    "暂不做",
    "不做",
    "不用做",
    "不需要做",
    "不需要",
    "无需",
    "先不要",
    "暂不要",
    "别做",
    "跳过",
    "暂缓",
    "不要跑",
    "不要运行",
    "不要执行",
    "别跑",
    "别运行",
    "别执行",
    "skip",
    "defer",
    "not needed",
    "no need",
    "don't do",
    "do not do",
    "don't run",
    "do not run",
    "don't execute",
    "do not execute",
];

/// Phrases that turn a surface negative into a demand or a prohibition of a
/// waiver. Checked before [`WAIVER_MARKERS`].
const WAIVER_GUARDS: &[&str] = &[
    "不要忘记",
    "别忘了",
    "别忘记",
    "不能忘记",
    "记得",
    "不要跳过",
    "不能跳过",
    "别跳过",
    "don't forget",
    "do not forget",
    "remember to",
    "don't skip",
    "do not skip",
    "never skip",
];

/// Effective requirements from the durable objective and user amendments.
///
/// A later user amendment may explicitly waive a requirement; that removal is
/// the user's authority and is applied here. No model-authored text reaches
/// this function — both inputs come from persisted user turns.
pub(crate) fn extract(objective: &str, amendments: &[String]) -> Vec<Requirement> {
    let mut collected: Vec<(String, RequirementKind, RequirementSource)> = Vec::new();
    collect_segments(objective, &RequirementSource::Objective, &mut collected);

    let mut tests_demanded = test_demanded(objective);
    for amendment in amendments {
        let segments = split_segments(amendment);
        // The user's waiver is authority: it removes a requirement before the
        // amendment's own deliverables are extracted. Scope the waiver to its
        // own clause: "B、C 暂时不做，这次只交付 A" must not waive A.
        for segment in &segments {
            for clause in segment.split([',', '，']) {
                if !is_waiver(clause) {
                    continue;
                }
                if mentions_tests(clause) {
                    tests_demanded = false;
                }
                collected.retain(|(text, kind, _)| {
                    !matches!(kind, RequirementKind::Tests) && !mentioned(text, clause)
                });
            }
        }
        let kept = segments
            .iter()
            .map(|segment| {
                let clauses: Vec<&str> = segment.split([',', '，']).collect();
                if clauses.len() <= 1 {
                    return segment.clone();
                }
                clauses
                    .into_iter()
                    .filter(|clause| !is_waiver(clause))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        if test_demanded(&kept) {
            tests_demanded = true;
        }
        collect_segments(
            &kept,
            &RequirementSource::Amendment(amendment.clone()),
            &mut collected,
        );
    }

    if tests_demanded {
        collected.push((
            "tests".to_string(),
            RequirementKind::Tests,
            RequirementSource::Objective,
        ));
    }

    let mut requirements: Vec<Requirement> = Vec::new();
    for (text, kind, source) in collected {
        let text = clean_token(&text);
        if text.is_empty() {
            continue;
        }
        let duplicate = requirements.iter().any(|requirement| {
            normalize(&requirement.text) == normalize(&text)
                && kind_key(requirement.kind) == kind_key(kind)
        });
        if duplicate {
            continue;
        }
        requirements.push(Requirement {
            id: format!("R{}", requirements.len() + 1),
            text,
            kind,
            source,
        });
    }
    requirements
}

/// Acceptance checks the model's `commit_task` declaration is missing.
///
/// Matching is token containment over the user's own wording: the model is
/// told to quote the requirement, and this check never judges whether a check
/// is adequate, only whether the deliverable disappeared.
pub(crate) fn uncovered(requirements: &[Requirement], acceptance: &[String]) -> Vec<String> {
    let checks: Vec<String> = acceptance.iter().map(|check| normalize(check)).collect();
    requirements
        .iter()
        .filter(|requirement| match requirement.kind {
            RequirementKind::Tests => !checks.iter().any(|check| mentions_tests(check)),
            _ => {
                let need = normalize(&requirement.text);
                !checks.iter().any(|check| covers(check, &need))
            }
        })
        .map(|requirement| requirement.text.clone())
        .collect()
}

/// Parse the optional `requirements` accounting array from `update_goal`.
pub(crate) fn parse_accounting(
    arguments: &serde_json::Value,
) -> Result<Option<Vec<RequirementAccount>>, String> {
    let Some(value) = arguments.get("requirements") else {
        return Ok(None);
    };
    let Some(items) = value.as_array() else {
        return Err(
            "`requirements` must be an array of {requirement, status, evidence} objects"
                .to_string(),
        );
    };
    if items.len() > 20 {
        return Err("`requirements` may list at most 20 entries".to_string());
    }
    let mut accounts = Vec::with_capacity(items.len());
    for item in items {
        let requirement = item
            .get("requirement")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.chars().count() <= 500)
            .ok_or_else(|| {
                "each `requirements` entry needs a non-empty `requirement` (up to 500 characters)"
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
            requirement: requirement.to_string(),
            status,
            evidence: evidence.to_string(),
        });
    }
    Ok(Some(accounts))
}

/// Decide whether the completion accounts for every explicit requirement.
pub(crate) fn decide(facts: &ScopeFacts<'_>) -> ScopeDecision {
    if facts.requirements.is_empty() {
        return ScopeDecision::Accept {
            warnings: Vec::new(),
        };
    }
    let accounting = facts.accounting.unwrap_or(&[]);
    let mut refusals: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    for requirement in facts.requirements {
        match requirement.kind {
            RequirementKind::Tests => {
                let Some(fresh) =
                    facts.runs.iter().rev().find(|run| {
                        crate::executor::goal_verification::is_test_command(&run.label)
                    })
                else {
                    refusals.push(
                        "the user explicitly requires the tests to pass, but no test command ran \
                         in this task"
                            .to_string(),
                    );
                    continue;
                };
                if !fresh.ok {
                    refusals.push(format!(
                        "the user explicitly requires the tests to pass, but `{}` failed (step {})",
                        fresh.label, fresh.model_step
                    ));
                } else if fresh.mutation_ops < facts.mutation_ops {
                    refusals.push(format!(
                        "the user explicitly requires the tests to pass, but `{}` passed before \
                         the most recent change (step {}); run the tests against the current \
                         workspace",
                        fresh.label, fresh.model_step
                    ));
                }
            }
            RequirementKind::Command => {
                let need = normalize(&requirement.text);
                let ran = facts.runs.iter().rev().any(|run| {
                    run.ok
                        && run.mutation_ops >= facts.mutation_ops
                        && normalize(&run.label).contains(&need)
                });
                if !ran {
                    refusals.push(format!(
                        "the user asked to run `{}`, but no successful run after the last change \
                         matches it",
                        requirement.text
                    ));
                }
            }
            RequirementKind::Artifact { removed } => {
                let Some(account) = find_account(accounting, requirement) else {
                    refusals.push(unaccounted(requirement));
                    continue;
                };
                match account.status {
                    AccountStatus::NotDone => refusals.push(format!(
                        "`{}` is accounted not_done ({})",
                        requirement.text, account.evidence
                    )),
                    AccountStatus::Unverifiable => match (facts.artifact)(&requirement.text) {
                        // `unverifiable` names a delivered result the runtime
                        // cannot prove. It is not an exemption from a fact the
                        // runtime CAN prove.
                        ArtifactState::Absent if !removed => refusals.push(format!(
                            "`{}` is accounted unverifiable, but it does not exist in the \
                             workspace and no recorded change created it",
                            requirement.text
                        )),
                        ArtifactState::Present if removed => refusals.push(format!(
                            "`{}` is accounted unverifiable, but it still exists",
                            requirement.text
                        )),
                        ArtifactState::Unknown => warnings.push(format!(
                            "requirement `{}` is explicitly unverifiable ({}); the completion \
                             carries an unverified boundary",
                            requirement.text, account.evidence
                        )),
                        _ => warnings.push(format!(
                            "requirement `{}` is explicitly unverifiable ({}); the completion \
                             carries an unverified boundary",
                            requirement.text, account.evidence
                        )),
                    },
                    AccountStatus::Done => match (facts.artifact)(&requirement.text) {
                        ArtifactState::Absent if !removed => refusals.push(format!(
                            "`{}` is accounted done, but it does not exist in the workspace and no \
                             recorded change created it",
                            requirement.text
                        )),
                        ArtifactState::Present if removed => refusals.push(format!(
                            "`{}` is accounted deleted, but it still exists",
                            requirement.text
                        )),
                        ArtifactState::Unknown => warnings.push(format!(
                            "requirement `{}` is accounted done, but the runtime could not inspect \
                             that path (outside the workspace or not resolvable); it is unverified",
                            requirement.text
                        )),
                        _ => {}
                    },
                }
            }
            RequirementKind::Named => {
                let Some(account) = find_account(accounting, requirement) else {
                    refusals.push(unaccounted(requirement));
                    continue;
                };
                match account.status {
                    AccountStatus::NotDone => refusals.push(format!(
                        "`{}` is accounted not_done ({})",
                        requirement.text, account.evidence
                    )),
                    AccountStatus::Unverifiable => warnings.push(format!(
                        "requirement `{}` is explicitly unverifiable ({}); the completion carries \
                         an unverified boundary",
                        requirement.text, account.evidence
                    )),
                    AccountStatus::Done => warnings.push(format!(
                        "requirement `{}` is reported done but has no mechanical check; the \
                         completion rests on the model's account, not runtime evidence",
                        requirement.text
                    )),
                }
            }
        }
    }

    if refusals.is_empty() {
        ScopeDecision::Accept { warnings }
    } else {
        ScopeDecision::Refuse(refusal_message(&refusals))
    }
}

fn refusal_message(refusals: &[String]) -> String {
    let mut message = String::from(
        "update_goal(complete) was refused: the user's stated scope is not fully accounted for.\n",
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
        "The goal remains active. Finish these requirements and call update_goal(status=\"complete\", \
         requirements=[...]) with one entry per requirement (requirement, status done|not_done|\
         unverifiable, evidence), or call update_goal(status=\"blocked\", ...) with the exact \
         obstruction. Do not narrow the user's scope to what you already did.",
    );
    message
}

fn unaccounted(requirement: &Requirement) -> String {
    format!(
        "`{}` is named in the user's request, but your completion did not account for it",
        requirement.text
    )
}

fn find_account<'a>(
    accounts: &'a [RequirementAccount],
    requirement: &Requirement,
) -> Option<&'a RequirementAccount> {
    let need = normalize(&requirement.text);
    if need.chars().count() < 2 {
        return None;
    }
    accounts
        .iter()
        .find(|account| covers(&normalize(&account.requirement), &need))
}

/// Whether an acceptance check names the requirement. The requirement must
/// appear in the check, never the reverse: a narrower check does not cover the
/// user's deliverable.
fn covers(check: &str, requirement: &str) -> bool {
    requirement.chars().count() >= 2 && check.contains(requirement)
}

fn mentions_tests(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("测试")
        || lower.contains("test")
        || lower.contains("pytest")
        || lower.contains("unittest")
        || lower.contains("jest")
        || lower.contains("vitest")
        || lower.contains("spec")
}

fn collect_segments(
    text: &str,
    source: &RequirementSource,
    out: &mut Vec<(String, RequirementKind, RequirementSource)>,
) {
    for segment in split_segments(text) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        collect_segment(segment, source, out);
    }
}

fn split_segments(text: &str) -> Vec<String> {
    text.split(|c: char| CLAUSE_BREAKS.contains(&c))
        .map(str::to_string)
        .collect()
}

fn collect_segment(
    segment: &str,
    source: &RequirementSource,
    out: &mut Vec<(String, RequirementKind, RequirementSource)>,
) {
    let segment = strip_fenced_blocks(segment);
    collect_backticks(&segment, source, out);
    collect_paths(&segment, source, out);
    collect_enumerations(&segment, source, out);
}

/// Fenced code is quoted material, not the user's instruction.
fn strip_fenced_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_block = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_block = !in_block;
            out.push('\n');
            continue;
        }
        if !in_block {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn collect_backticks(
    segment: &str,
    source: &RequirementSource,
    out: &mut Vec<(String, RequirementKind, RequirementSource)>,
) {
    let parts: Vec<&str> = segment.split('`').collect();
    for span in parts.iter().skip(1).step_by(2) {
        let span = span.trim();
        if span.is_empty() || span.chars().count() > 120 {
            continue;
        }
        let mut added_path = false;
        for token in span.split_whitespace() {
            let token = clean_token(token);
            if !path_like(&token) {
                continue;
            }
            added_path = true;
            let kind = if executable_path(&token)
                && (has_any(span, RUN_VERBS) || has_any(segment, RUN_VERBS))
            {
                RequirementKind::Command
            } else {
                RequirementKind::Artifact { removed: false }
            };
            out.push((token, kind, source.clone()));
        }
        if added_path || is_test_phrase(span) {
            continue;
        }
        // A backticked command the user asked to run is mechanical evidence,
        // not a named deliverable. A test command is already the Tests
        // requirement; a non-test command must have run.
        if crate::executor::goal_verification::is_test_command(span) {
            continue;
        }
        if command_text(span) {
            out.push((span.to_string(), RequirementKind::Command, source.clone()));
            continue;
        }
        out.push((span.to_string(), RequirementKind::Named, source.clone()));
    }
}

fn collect_paths(
    segment: &str,
    source: &RequirementSource,
    out: &mut Vec<(String, RequirementKind, RequirementSource)>,
) {
    for raw in segment.split(|c: char| TOKEN_BREAKS.contains(&c)) {
        let token = clean_token(raw);
        if !path_like(&token) || looks_like_url(&token) {
            continue;
        }
        // A path mentioned in a prohibition is a constraint, not a deliverable.
        if prohibition_around(segment, &token) {
            continue;
        }
        let clause = clause_around(segment, &token);
        let removed = has_any(clause, REMOVAL_VERBS) && !has_any(clause, NEGATIONS);
        let kind = if removed {
            RequirementKind::Artifact { removed: true }
        } else if executable_path(&token) && has_any(clause, RUN_VERBS) {
            RequirementKind::Command
        } else {
            RequirementKind::Artifact { removed: false }
        };
        out.push((token, kind, source.clone()));
    }
}

fn collect_enumerations(
    segment: &str,
    source: &RequirementSource,
    out: &mut Vec<(String, RequirementKind, RequirementSource)>,
) {
    let mut items: Vec<String> = Vec::new();
    if let Some(item) = bullet_item(segment) {
        items.push(item);
    }
    // A colon preamble ("完成全部三个阶段：...") names the category, not an
    // item; the list itself starts after the colon.
    let enumerated = match segment.rsplit_once(['：', ':']) {
        Some((head, tail)) if has_any(head, DELIVERY_VERBS) => tail,
        _ => segment,
    };
    let ideographic: Vec<&str> = enumerated.split('、').collect();
    if ideographic.len() >= 2 {
        items.extend(ideographic.iter().map(|item| (*item).to_string()));
    }
    if let Some((head, tail)) = segment.rsplit_once(['：', ':'])
        && ideographic.len() < 2
        && has_any(head, DELIVERY_VERBS)
    {
        let comma_items: Vec<&str> = tail.split([',', '，']).collect();
        if (2..=6).contains(&comma_items.len())
            && comma_items
                .iter()
                .all(|item| item.trim().chars().count() <= 40)
        {
            items.extend(comma_items.iter().map(|item| (*item).to_string()));
        }
    }

    for item in items {
        let Some(text) = clean_item(&item) else {
            continue;
        };
        if path_like(&text) || contains_path(&text) || text.contains('`') {
            continue;
        }
        if is_test_phrase(&text)
            || crate::executor::goal_verification::is_test_command(&text)
            || command_text(&text)
        {
            continue;
        }
        out.push((text, RequirementKind::Named, source.clone()));
    }
}

/// Distinguishes `Artifact { removed: true }` from `Artifact { removed: false }`;
/// `mem::discriminant` would collapse them because the variant is the same.
fn kind_key(kind: RequirementKind) -> u8 {
    match kind {
        RequirementKind::Artifact { removed: false } => 0,
        RequirementKind::Artifact { removed: true } => 1,
        RequirementKind::Command => 2,
        RequirementKind::Named => 3,
        RequirementKind::Tests => 4,
    }
}

fn has_any(text: &str, needles: &[&str]) -> bool {
    let lower = text.to_lowercase();
    needles.iter().any(|needle| lower.contains(needle))
}

fn is_waiver(segment: &str) -> bool {
    if has_any(segment, WAIVER_GUARDS) {
        return false;
    }
    has_any(segment, WAIVER_MARKERS)
}

fn test_demanded(text: &str) -> bool {
    for segment in split_segments(text) {
        if is_waiver(&segment) && mentions_tests(&segment) {
            continue;
        }
        if segment_is_negated(&segment) {
            continue;
        }
        // Paths such as `tests/foo.rs` are deliverables, not a demand that the
        // suite passes. Remove path-like tokens before reading the prose.
        let prose: String = segment
            .split(|c: char| TOKEN_BREAKS.contains(&c))
            .filter(|token| !path_like(&clean_token(token)))
            .collect::<Vec<_>>()
            .join(" ");
        let lower = prose.to_lowercase();
        if lower.contains("cargo test")
            || lower.contains("go test")
            || lower.contains("npm test")
            || lower.contains("pnpm test")
            || lower.contains("yarn test")
            || lower.contains("mvn test")
            || lower.contains("pytest")
            || lower.contains("unittest")
            || lower.contains("ctest")
            || lower.contains("jest")
            || lower.contains("vitest")
        {
            return true;
        }
        if has_any(
            &segment,
            &[
                "不要跳过",
                "不能跳过",
                "别跳过",
                "don't skip",
                "do not skip",
                "never skip",
                "必须测试",
                "must test",
            ],
        ) {
            return true;
        }
        if !mentions_tests(&prose) {
            continue;
        }
        const DEMAND: &[&str] = &[
            "通过",
            "运行",
            "执行",
            "跑",
            "全部",
            "所有",
            "完整",
            "必须",
            "需要",
            "确保",
            "修复",
            "实现",
            "完成",
            "新增",
            "添加",
            "修改",
            "更新",
            "pass",
            "run",
            "all",
            "must",
            "should",
            "suite",
            "green",
            "fix",
            "implement",
            "complete",
            "add",
            "update",
        ];
        if has_any(&prose, DEMAND) {
            return true;
        }
    }
    false
}

fn segment_is_negated(segment: &str) -> bool {
    if has_any(
        segment,
        &[
            "不要忘记",
            "别忘了",
            "别忘记",
            "记得",
            "don't forget",
            "do not forget",
            "remember to",
            "不要跳过",
            "不能跳过",
            "别跳过",
            "don't skip",
            "do not skip",
            "never skip",
        ],
    ) {
        return false;
    }
    has_any(
        segment,
        &[
            "不要",
            "不用",
            "无需",
            "不需要",
            "先不",
            "暂不",
            "别",
            "跳过",
            "禁止",
            "skip",
            "don't",
            "do not",
            "not needed",
            "defer",
        ],
    )
}

fn is_test_phrase(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower == "tests" || lower == "test" || lower == "测试" || lower == "测试通过"
}

fn prohibition_around(segment: &str, token: &str) -> bool {
    let clause = clause_around(segment, token);
    has_any(clause, NEGATIONS) && has_any(clause, MUTATION_VERBS)
}

fn clause_around<'a>(segment: &'a str, token: &str) -> &'a str {
    for clause in segment.split([',', '，']) {
        if clause.contains(token) {
            return clause;
        }
    }
    segment
}

fn mentioned(text: &str, clause: &str) -> bool {
    let text = normalize(text);
    let clause = normalize(clause);
    text.chars().count() >= 2 && clause.contains(&text)
}

/// Whether a span starts with a recognised command program. The user's own
/// backticks are the only thing that turns a word into a command here; the
/// runtime does not guess from prose.
fn command_text(text: &str) -> bool {
    const PROGRAMS: &[&str] = &[
        "cargo",
        "go",
        "npm",
        "pnpm",
        "yarn",
        "bun",
        "deno",
        "make",
        "just",
        "bazel",
        "mvn",
        "gradle",
        "dotnet",
        "swift",
        "flutter",
        "dart",
        "mix",
        "pytest",
        "phpunit",
        "python",
        "python3",
        "py",
        "node",
        "sh",
        "bash",
        "zsh",
        "pwsh",
        "powershell",
        "cmd",
        "ctest",
        "rustfmt",
        "gofmt",
        "golangci-lint",
        "eslint",
        "prettier",
        "tsc",
        "ruff",
        "mypy",
        "black",
        "git",
    ];
    let first = text.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    PROGRAMS.contains(&base)
}

fn path_like(token: &str) -> bool {
    let token = clean_token(token);
    if token.is_empty() || looks_like_url(&token) {
        return false;
    }
    if token.contains('/') || token.contains('\\') {
        return !token.contains("//");
    }
    let Some((stem, extension)) = token.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty() && PATH_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
}

fn executable_path(token: &str) -> bool {
    const EXECUTABLE: &[&str] = &[
        "sh", "bash", "zsh", "ps1", "bat", "cmd", "py", "rb", "pl", "php", "js", "mjs", "cjs",
    ];
    token
        .rsplit_once('.')
        .map(|(_, extension)| EXECUTABLE.contains(&extension.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn looks_like_url(token: &str) -> bool {
    token.contains("://") || token.starts_with("http") || token.starts_with("www.")
}

fn contains_path(text: &str) -> bool {
    text.split(|c: char| TOKEN_BREAKS.contains(&c))
        .any(|token| path_like(&clean_token(token)))
}

fn clean_token(token: &str) -> String {
    token
        .trim()
        .trim_matches(|c: char| {
            c.is_whitespace() || "`'\"“”‘’。，,、；;：:!?！？()（）[]【】{}<>".contains(c)
        })
        .to_string()
}

fn bullet_item(segment: &str) -> Option<String> {
    let trimmed = segment.trim_start();
    for marker in ["- ", "* ", "• ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest.trim_start().to_string());
        }
    }
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() {
        let rest = &trimmed[digits.len()..];
        for marker in [". ", ") ", "、", ".", ")"] {
            if let Some(rest) = rest.strip_prefix(marker) {
                return Some(rest.trim_start().to_string());
            }
        }
    }
    for (open, close) in [('(', ')'), ('（', '）')] {
        if let Some(rest) = trimmed.strip_prefix(open) {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty()
                && let Some(item) = rest[digits.len()..].strip_prefix(close)
            {
                return Some(item.trim_start().to_string());
            }
        }
    }
    None
}

fn clean_item(raw: &str) -> Option<String> {
    // Keep only the leading clause of an item: a following comma usually
    // starts a new instruction, not part of the deliverable's name.
    let head = raw
        .split(['，', ',', '。', '；', ';', '：', ':'])
        .next()
        .unwrap_or(raw);
    let head = match head.rsplit_once(['：', ':']) {
        Some((before, after)) if has_any(before, DELIVERY_VERBS) => after.trim(),
        _ => head.trim(),
    };
    let mut text = head.trim_matches(|c: char| {
        c.is_whitespace() || "`'\"“”‘’、。；;!?！？()（）[]【】".contains(c)
    });
    for verb in LEADING_VERBS {
        if let Some(rest) = text.strip_prefix(verb) {
            let rest = rest.trim_start_matches(|c: char| {
                c.is_whitespace() || c == '：' || c == ':' || c == '的'
            });
            if !rest.is_empty() {
                text = rest;
            }
            break;
        }
    }
    let text = text.trim_matches(|c: char| {
        c.is_whitespace() || "`'\"“”‘’、。；;!?！？()（）[]【】".contains(c)
    });
    let count = text.chars().count();
    if !(2..=60).contains(&count) {
        return None;
    }
    if !text.chars().any(|c| c.is_alphanumeric() || is_cjk(c)) {
        return None;
    }
    if is_waiver(text) {
        return None;
    }
    Some(text.to_string())
}

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
        || ('\u{3400}'..='\u{4dbf}').contains(&c)
        || ('\u{f900}'..='\u{faff}').contains(&c)
}

fn normalize(text: &str) -> String {
    let text = clean_token(text).trim_start_matches("./").to_lowercase();
    text.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(label: &str, ok: bool, step: u32, mutation_ops: u64) -> VerificationRun {
        VerificationRun {
            label: label.to_string(),
            ok,
            model_step: step,
            mutation_ops,
        }
    }

    fn req(text: &str, kind: RequirementKind) -> Requirement {
        Requirement {
            id: "R1".to_string(),
            text: text.to_string(),
            kind,
            source: RequirementSource::Objective,
        }
    }

    fn texts(requirements: &[Requirement]) -> Vec<&str> {
        requirements
            .iter()
            .map(|requirement| requirement.text.as_str())
            .collect()
    }

    fn account(text: &str, status: AccountStatus, evidence: &str) -> RequirementAccount {
        RequirementAccount {
            requirement: text.to_string(),
            status,
            evidence: evidence.to_string(),
        }
    }

    /// The exact DF-22 shape: three named stage files plus a named command.
    #[test]
    fn three_stage_prompt_yields_each_deliverable() {
        let requirements = extract(
            "完成全部三个阶段：创建 stage-1.txt、stage-2.txt、stage-3.txt，\
             最后运行 sh ./verify.sh，全部做完再结束。",
            &[],
        );
        let found = texts(&requirements);
        for expected in ["stage-1.txt", "stage-2.txt", "stage-3.txt", "./verify.sh"] {
            assert!(found.contains(&expected), "missing {expected}: {found:?}");
        }
        assert!(
            requirements.iter().any(|requirement| {
                requirement.text == "./verify.sh" && requirement.kind == RequirementKind::Command
            }),
            "a named script the user asked to run is a command requirement: {requirements:?}"
        );
    }

    #[test]
    fn a_module_list_yields_named_requirements() {
        let requirements = extract(
            "实现认证模块、订单模块、财务模块，并补齐所有相关测试。",
            &[],
        );
        let found = texts(&requirements);
        for expected in ["认证模块", "订单模块", "财务模块"] {
            assert!(found.contains(&expected), "missing {expected}: {found:?}");
        }
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "an explicit demand for tests is a requirement: {requirements:?}"
        );
    }

    #[test]
    fn open_ended_prose_yields_no_requirements() {
        assert!(
            extract("优化这个模块的架构，让它更容易维护。", &[]).is_empty(),
            "the runtime must not guess at semantic goals"
        );
        assert!(
            extract("这个函数为什么这么慢？", &[]).is_empty(),
            "an ordinary question is not a deliverable"
        );
        assert!(
            extract("帮我检查一下登录流程，看看有没有风险。", &[]).is_empty(),
            "open-ended investigation is not an enumerated deliverable"
        );
    }

    #[test]
    fn a_test_demand_is_recognized_and_its_negation_is_not() {
        let demanded = extract("实现全部接口，并通过完整测试。", &[]);
        assert!(
            demanded
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "{demanded:?}"
        );

        let waived = extract("继续，但是先不要跑测试。", &[]);
        assert!(
            !waived
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "an explicit waiver must not become a requirement: {waived:?}"
        );

        let command = extract("实现接口并运行 cargo test。", &[]);
        assert!(
            command
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "an explicitly named test command is a test demand: {command:?}"
        );
    }

    #[test]
    fn a_demand_to_not_forget_tests_is_not_a_waiver() {
        for text in [
            "不要忘记跑测试，并创建 stage-1.txt。",
            "别忘了测试通过后再结束。",
            "Don't forget to run the tests and create stage-1.txt.",
        ] {
            let requirements = extract(text, &[]);
            assert!(
                requirements
                    .iter()
                    .any(|requirement| requirement.kind == RequirementKind::Tests),
                "a reminder is a demand, not a waiver: {text} -> {requirements:?}"
            );
        }
        for text in ["不要跳过测试。", "Don't skip the tests."] {
            let requirements = extract(text, &[]);
            assert!(
                requirements
                    .iter()
                    .any(|requirement| requirement.kind == RequirementKind::Tests),
                "a prohibition of skipping is a demand: {text} -> {requirements:?}"
            );
        }
        for text in [
            "不要跑测试，只实现接口。",
            "Don't run the tests; implement only.",
        ] {
            let requirements = extract(text, &[]);
            assert!(
                !requirements
                    .iter()
                    .any(|requirement| requirement.kind == RequirementKind::Tests),
                "an explicit do-not-run is a waiver: {text} -> {requirements:?}"
            );
        }
    }

    #[test]
    fn a_later_user_waiver_removes_the_named_deliverable() {
        let objective = "完成全部三个阶段：创建 stage-1.txt、stage-2.txt、stage-3.txt，\
                         最后运行 sh ./verify.sh，全部做完再结束。";
        let requirements = extract(
            objective,
            &["stage-2.txt、stage-3.txt 暂时不做，这次只交付 stage-1.txt。".to_string()],
        );
        let found = texts(&requirements);
        assert!(found.contains(&"stage-1.txt"), "{found:?}");
        assert!(!found.contains(&"stage-2.txt"), "waived: {found:?}");
        assert!(!found.contains(&"stage-3.txt"), "waived: {found:?}");
        assert!(found.contains(&"./verify.sh"), "{found:?}");
    }

    #[test]
    fn a_waiver_of_tests_removes_the_test_requirement() {
        let requirements = extract(
            "实现全部接口并通过测试。",
            &["这次先不要跑测试，只实现接口。".to_string()],
        );
        assert!(
            !requirements
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "{requirements:?}"
        );
    }

    #[test]
    fn a_backticked_flag_is_an_explicit_named_requirement() {
        let requirements = extract("给 CLI 增加 `--verbose` 参数，并更新 `README`。", &[]);
        let found = texts(&requirements);
        assert!(found.contains(&"--verbose"), "{found:?}");
        assert!(found.contains(&"README"), "{found:?}");
    }

    #[test]
    fn a_backticked_command_is_mechanical_not_a_named_deliverable() {
        let requirements = extract("实现接口，最后运行 `cargo fmt --check`。", &[]);
        assert!(
            requirements.iter().any(|requirement| {
                requirement.text == "cargo fmt --check"
                    && requirement.kind == RequirementKind::Command
            }),
            "{requirements:?}"
        );

        let tests = extract("实现接口，最后运行 `python3 -m unittest`。", &[]);
        assert!(
            tests
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "{tests:?}"
        );
        assert!(
            !tests.iter().any(|requirement| {
                requirement.text.contains("unittest") && requirement.kind == RequirementKind::Named
            }),
            "the test command must not also become a named deliverable: {tests:?}"
        );
    }

    #[test]
    fn a_numbered_list_is_enumerated() {
        let requirements = extract("1. 认证模块\n2. 订单模块\n3. 财务模块\n", &[]);
        let found = texts(&requirements);
        for expected in ["认证模块", "订单模块", "财务模块"] {
            assert!(found.contains(&expected), "missing {expected}: {found:?}");
        }
    }

    #[test]
    fn a_path_in_a_prohibition_is_not_a_requirement() {
        let requirements = extract("不要修改 src/legacy.rs，只改 src/new.rs。", &[]);
        let found = texts(&requirements);
        assert!(!found.contains(&"src/legacy.rs"), "{found:?}");
        assert!(found.contains(&"src/new.rs"), "{found:?}");
    }

    #[test]
    fn a_removal_requirement_expects_absence() {
        let requirements = extract("删除 old.txt。", &[]);
        assert!(
            requirements.iter().any(|requirement| {
                requirement.text == "old.txt"
                    && requirement.kind == RequirementKind::Artifact { removed: true }
            }),
            "{requirements:?}"
        );
    }

    #[test]
    fn commit_acceptance_must_cover_every_named_deliverable() {
        let requirements = extract(
            "完成全部三个阶段：创建 stage-1.txt、stage-2.txt、stage-3.txt。",
            &[],
        );
        let missing = uncovered(&requirements, &["stage-1.txt 存在".to_string()]);
        assert!(missing.contains(&"stage-2.txt".to_string()), "{missing:?}");
        assert!(missing.contains(&"stage-3.txt".to_string()), "{missing:?}");

        let missing = uncovered(
            &requirements,
            &[
                "stage-1.txt 存在".to_string(),
                "stage-2.txt 存在".to_string(),
                "stage-3.txt 存在".to_string(),
            ],
        );
        assert!(missing.is_empty(), "{missing:?}");
    }

    #[test]
    fn extra_acceptance_checks_do_not_remove_a_user_requirement() {
        let requirements = extract("创建 a.txt、b.txt。", &[]);
        let missing = uncovered(
            &requirements,
            &[
                "a.txt 存在".to_string(),
                "b.txt 存在".to_string(),
                "cargo fmt --check 通过".to_string(),
            ],
        );
        assert!(missing.is_empty(), "extra checks are allowed: {missing:?}");
    }

    #[test]
    fn accounting_parses_only_the_three_honest_states() {
        let parsed = parse_accounting(&serde_json::json!({
            "requirements": [
                {"requirement": "stage-1.txt", "status": "done", "evidence": "created it"},
                {"requirement": "stage-2.txt", "status": "not_done", "evidence": "not reached"},
                {"requirement": "quality", "status": "unverifiable", "evidence": "semantic"}
            ]
        }))
        .unwrap()
        .unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].status, AccountStatus::Done);
        assert_eq!(parsed[1].status, AccountStatus::NotDone);
        assert_eq!(parsed[2].status, AccountStatus::Unverifiable);

        assert!(parse_accounting(&serde_json::json!({})).unwrap().is_none());
        assert!(
            parse_accounting(&serde_json::json!({
                "requirements": [{"requirement": "x", "status": "skipped", "evidence": "no"}]
            }))
            .is_err()
        );
        assert!(
            parse_accounting(&serde_json::json!({
                "requirements": [{"requirement": "x", "status": "done", "evidence": "  "}]
            }))
            .is_err(),
            "a claim without evidence is malformed"
        );
    }

    #[test]
    fn an_unaccounted_requirement_refuses_completion() {
        let requirements = vec![
            req("stage-1.txt", RequirementKind::Artifact { removed: false }),
            req("stage-2.txt", RequirementKind::Artifact { removed: false }),
        ];
        let present = |_: &str| ArtifactState::Present;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account("stage-1.txt", AccountStatus::Done, "created")]),
            runs: &[],
            mutation_ops: 1,
            artifact: &present,
        });
        match decision {
            ScopeDecision::Refuse(message) => {
                assert!(message.contains("stage-2.txt"), "{message}");
                assert!(!message.contains("stage-1.txt"), "{message}");
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_not_done_requirement_refuses_completion() {
        let requirements = vec![req(
            "stage-2.txt",
            RequirementKind::Artifact { removed: false },
        )];
        let present = |_: &str| ArtifactState::Present;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account(
                "stage-2.txt",
                AccountStatus::NotDone,
                "not reached",
            )]),
            runs: &[],
            mutation_ops: 1,
            artifact: &present,
        });
        assert!(matches!(
            &decision,
            ScopeDecision::Refuse(message) if message.contains("not_done")
        ));
    }

    /// The exact false completion: the model says it wrote stage-2, but no
    /// such artifact exists and no recorded change touched it.
    #[test]
    fn a_done_artifact_claim_without_the_artifact_is_refused() {
        let requirements = vec![req(
            "stage-2.txt",
            RequirementKind::Artifact { removed: false },
        )];
        let absent = |_: &str| ArtifactState::Absent;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account("stage-2.txt", AccountStatus::Done, "created it")]),
            runs: &[],
            mutation_ops: 1,
            artifact: &absent,
        });
        assert!(
            matches!(&decision, ScopeDecision::Refuse(message) if message.contains("does not exist")),
            "{decision:?}"
        );
    }

    #[test]
    fn an_artifact_that_already_existed_satisfies_the_claim() {
        let requirements = vec![req(
            "stage-1.txt",
            RequirementKind::Artifact { removed: false },
        )];
        let present = |_: &str| ArtifactState::Present;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account("stage-1.txt", AccountStatus::Done, "verified")]),
            runs: &[],
            mutation_ops: 0,
            artifact: &present,
        });
        assert_eq!(decision, ScopeDecision::Accept { warnings: vec![] });
    }

    #[test]
    fn a_removed_artifact_that_still_exists_is_refused() {
        let requirements = vec![req("old.txt", RequirementKind::Artifact { removed: true })];
        let present = |_: &str| ArtifactState::Present;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account("old.txt", AccountStatus::Done, "deleted")]),
            runs: &[],
            mutation_ops: 1,
            artifact: &present,
        });
        assert!(
            matches!(&decision, ScopeDecision::Refuse(message) if message.contains("still exists")),
            "{decision:?}"
        );
    }

    #[test]
    fn an_unknown_artifact_state_completes_as_an_unverified_boundary() {
        let requirements = vec![req(
            "src/outside.rs",
            RequirementKind::Artifact { removed: false },
        )];
        let unknown = |_: &str| ArtifactState::Unknown;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account("src/outside.rs", AccountStatus::Done, "edited")]),
            runs: &[],
            mutation_ops: 1,
            artifact: &unknown,
        });
        match decision {
            ScopeDecision::Accept { warnings } => {
                assert!(
                    warnings
                        .iter()
                        .any(|warning| warning.contains("src/outside.rs")),
                    "{warnings:?}"
                );
            }
            other => panic!("expected an unverified acceptance, got {other:?}"),
        }
    }

    #[test]
    fn an_unverifiable_claim_cannot_hide_a_missing_artifact() {
        let requirements = vec![req(
            "stage-2.txt",
            RequirementKind::Artifact { removed: false },
        )];
        let absent = |_: &str| ArtifactState::Absent;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account(
                "stage-2.txt",
                AccountStatus::Unverifiable,
                "the content is hard to check",
            )]),
            runs: &[],
            mutation_ops: 1,
            artifact: &absent,
        });
        assert!(
            matches!(&decision, ScopeDecision::Refuse(message) if message.contains("does not exist")),
            "{decision:?}"
        );
    }

    #[test]
    fn a_named_requirement_completes_only_with_an_unverified_boundary() {
        let requirements = vec![req("认证模块", RequirementKind::Named)];
        let present = |_: &str| ArtifactState::Present;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account(
                "认证模块",
                AccountStatus::Done,
                "implemented and tested",
            )]),
            runs: &[],
            mutation_ops: 3,
            artifact: &present,
        });
        match decision {
            ScopeDecision::Accept { warnings } => assert!(
                warnings.iter().any(|warning| warning.contains("认证模块")),
                "{warnings:?}"
            ),
            other => panic!("expected an unverified acceptance, got {other:?}"),
        }
    }

    #[test]
    fn an_unverifiable_requirement_is_a_warning_not_a_claim_of_proof() {
        let requirements = vec![req("可维护性", RequirementKind::Named)];
        let present = |_: &str| ArtifactState::Present;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account(
                "可维护性",
                AccountStatus::Unverifiable,
                "a semantic property; the change is described and tested but not provable here",
            )]),
            runs: &[],
            mutation_ops: 3,
            artifact: &present,
        });
        match decision {
            ScopeDecision::Accept { warnings } => assert!(
                warnings.iter().any(|warning| warning.contains("unverifiable")
                    && warning.contains("可维护性")),
                "{warnings:?}"
            ),
            other => panic!("expected an accepted boundary, got {other:?}"),
        }
    }

    #[test]
    fn a_user_required_test_must_pass_after_the_last_change() {
        let requirements = vec![req("tests", RequirementKind::Tests)];
        let present = |_: &str| ArtifactState::Present;
        let refuse = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: None,
            runs: &[run("cargo check", true, 2, 5)],
            mutation_ops: 5,
            artifact: &present,
        });
        assert!(
            matches!(&refuse, ScopeDecision::Refuse(message) if message.contains("test")),
            "{refuse:?}"
        );

        let stale = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: None,
            runs: &[run("cargo test", true, 2, 4)],
            mutation_ops: 5,
            artifact: &present,
        });
        assert!(
            matches!(&stale, ScopeDecision::Refuse(message) if message.contains("before the most recent")),
            "{stale:?}"
        );

        let accept = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: None,
            runs: &[run("cargo test", true, 6, 5)],
            mutation_ops: 5,
            artifact: &present,
        });
        assert_eq!(accept, ScopeDecision::Accept { warnings: vec![] });
    }

    #[test]
    fn a_named_command_must_have_run_after_the_last_change() {
        let requirements = vec![req("./verify.sh", RequirementKind::Command)];
        let present = |_: &str| ArtifactState::Present;
        let refuse = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: None,
            runs: &[],
            mutation_ops: 2,
            artifact: &present,
        });
        assert!(
            matches!(&refuse, ScopeDecision::Refuse(message) if message.contains("./verify.sh")),
            "{refuse:?}"
        );

        let accept = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: None,
            runs: &[run("sh ./verify.sh", true, 9, 2)],
            mutation_ops: 2,
            artifact: &present,
        });
        assert_eq!(accept, ScopeDecision::Accept { warnings: vec![] });
    }

    #[test]
    fn no_explicit_requirements_leave_ordinary_goals_unchanged() {
        let present = |_: &str| ArtifactState::Present;
        assert_eq!(
            decide(&ScopeFacts {
                requirements: &[],
                accounting: None,
                runs: &[],
                mutation_ops: 0,
                artifact: &present,
            }),
            ScopeDecision::Accept { warnings: vec![] }
        );
    }
}
