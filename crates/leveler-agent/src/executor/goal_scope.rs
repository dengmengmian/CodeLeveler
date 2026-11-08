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
    /// The user forbade something (`禁止实现支付功能`, `不要新增第三方登录`).
    ///
    /// A prohibition is a constraint on HOW the work is done, never a
    /// deliverable: it has no artifact to produce and no mechanical check that
    /// could prove it, so it can neither be covered by an acceptance check nor
    /// be accounted as `done`. It stays in the requirement set because the user
    /// said it and the model must still see it, and it is surfaced to the model
    /// as a constraint rather than as work.
    Forbidden,
}

impl RequirementKind {
    /// Whether this kind is a deliverable the runtime can hold the model to.
    ///
    /// Only `false` for [`Self::Forbidden`]: everything else names something
    /// the user asked to exist, run, or pass.
    pub(crate) fn is_deliverable(self) -> bool {
        !matches!(self, Self::Forbidden)
    }

    /// Human-readable class for tool results and diagnostics.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Artifact { removed: false } => "artifact",
            Self::Artifact { removed: true } => "absent-artifact",
            Self::Command => "command",
            Self::Named => "deliverable",
            Self::Tests => "tests",
            Self::Forbidden => "forbidden",
        }
    }
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
        // A question is not a scope instruction by itself. "测试通过了吗？" asks
        // about the work; reading it as a NEW test demand would let a status
        // question change the acceptance the paused goal must still satisfy.
        // But a request keeps its authority in question form ("能不能顺便把
        // X 也补上？"), so the message is read clause by clause: the query part
        // adds nothing, the request part still does. A waiver is only read
        // from a message that does not ask — "阶段 2 可以先不做吗？" is not a
        // waiver until the user says so, and dropping a requirement on a
        // question would weaken acceptance without the user's authority. The
        // objective itself is always a request, so only later amendments are
        // split here.
        let segments: Vec<String> = split_segments(amendment)
            .into_iter()
            .filter(|segment| !question_form(segment))
            .collect();
        let asked: Vec<String> = split_segments(amendment)
            .into_iter()
            .filter(|segment| question_form(segment))
            .flat_map(|segment| request_clauses(&segment))
            .collect();
        if segments.is_empty() && asked.is_empty() {
            continue;
        }
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
            .chain(asked)
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
    } else {
        // A demand the runtime cannot enumerate must not be silently dropped,
        // and it must not be replaced by a narrower mechanical one. The
        // subject stays in the requirement set: the acceptance list has to
        // name it and the completion account has to cover it, while the record
        // states that the coverage is the model's account, not proof.
        for text in std::iter::once(objective).chain(amendments.iter().map(String::as_str)) {
            if let Some(subject) = verification_subject(text) {
                collected.push((
                    subject,
                    RequirementKind::Named,
                    RequirementSource::Objective,
                ));
            }
        }
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
        // One artifact cannot be both required and required-absent. When the
        // same path appears under opposite verbs, the LATER statement is the
        // user's final word (`create a.txt. delete a.txt.` asks for absence),
        // so the earlier claim is replaced instead of kept beside it. Keeping
        // both made the gate unsatisfiable and the run loop (DF-06).
        if let Some(existing) = requirements.iter_mut().find(|requirement| {
            normalize(&requirement.text) == normalize(&text)
                && matches!(requirement.kind, RequirementKind::Artifact { .. })
                && matches!(kind, RequirementKind::Artifact { .. })
        }) {
            existing.kind = kind;
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

/// Whether recording `text` as one more user amendment changes the goal's
/// effective requirement set.
///
/// This is the runtime's ONE reading of "does this message name work?", and it
/// is the same [`extract`] the acceptance gate uses. A status question, a
/// consultation, a bare `继续` and a restatement all leave the set identical.
/// A new deliverable, a prohibition, or an explicit waiver changes it.
///
/// It decides scope only. It never authorizes execution, never releases a
/// pause, and never judges completion.
pub(crate) fn changes_scope(objective: &str, amendments: &[String], text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let mut extended = amendments.to_vec();
    extended.push(text.to_string());
    requirement_set(&extract(objective, amendments))
        != requirement_set(&extract(objective, &extended))
}

/// Whether one user message must join the goal's durable instruction
/// sequence, as the instruction of a GOAL window.
///
/// That sequence is the user's own words, and the runtime reads two different
/// facts from it:
///
/// - SCOPE ([`changes_scope`]): the requirements the acceptance gate holds the
///   model to, including a prohibition the user stated while addressing the
///   goal;
/// - AUTHORIZATION ([`leveler_lifecycle::latest_authorization_signal`]): the
///   user's newest pause or continuation, which is the only thing that
///   releases a pause the user set.
///
/// A status question is neither, so it is not recorded and cannot pollute
/// either reading.
pub(crate) fn is_goal_instruction(objective: &str, amendments: &[String], text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    changes_scope(objective, amendments, text) || changes_authorization(objective, amendments, text)
}

/// Whether one CHAT message joins the durable instruction sequence of the goal
/// this task still owes.
///
/// A Chat turn is not the goal's window. The user can ask a side question, or
/// state a constraint for that one request (`只回答，不要改任何文件`), without
/// binding the goal to it forever — a prohibition remembered as the goal's own
/// would later stop the goal's authorized work. So a Chat message joins the
/// goal only when it changes the DELIVERABLE set: work added, or work the user
/// explicitly waived. Anything else still reaches the model inside that turn,
/// and the model records it by committing the work (`commit_task`), which goes
/// through the goal-window reading above.
pub(crate) fn is_chat_goal_instruction(objective: &str, amendments: &[String], text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    changes_work(objective, amendments, text) || changes_authorization(objective, amendments, text)
}

/// Whether the newest pause/continuation directive changed.
fn changes_authorization(objective: &str, amendments: &[String], text: &str) -> bool {
    let signal = |extra: Option<&str>| {
        leveler_lifecycle::latest_authorization_signal(
            std::iter::once(objective)
                .chain(amendments.iter().map(String::as_str))
                .chain(extra),
        )
    };
    signal(None) != signal(Some(text))
}

/// Whether the DELIVERABLE part of the requirement set changed. A prohibition
/// alone does not count: it constrains how work is done, not what is owed.
pub(crate) fn changes_work(objective: &str, amendments: &[String], text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    let mut extended = amendments.to_vec();
    extended.push(text.to_string());
    deliverable_set(&extract(objective, amendments))
        != deliverable_set(&extract(objective, &extended))
}

/// The identity of a requirement set, ignoring the positional `R<n>` ids that
/// [`extract`] assigns.
fn requirement_set(requirements: &[Requirement]) -> Vec<(String, &'static str)> {
    let mut set: Vec<(String, &'static str)> = requirements
        .iter()
        .map(|requirement| (normalize(&requirement.text), requirement.kind.label()))
        .collect();
    set.sort();
    set
}

/// The deliverables of a requirement set, ignoring positional ids and
/// prohibitions.
fn deliverable_set(requirements: &[Requirement]) -> Vec<(String, &'static str)> {
    let mut set: Vec<(String, &'static str)> = requirements
        .iter()
        .filter(|requirement| requirement.kind.is_deliverable())
        .map(|requirement| (normalize(&requirement.text), requirement.kind.label()))
        .collect();
    set.sort();
    set
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
            // A prohibition has no deliverable to name; requiring a check for
            // it would ask the model to "prove" a negative and made
            // `commit_task` refuse honest contracts for things the user asked
            // NOT to be built.
            RequirementKind::Forbidden => false,
            RequirementKind::Tests => !checks.iter().any(|check| mentions_tests(check)),
            RequirementKind::Named => !covers_named(&checks, &requirement.text),
            _ => {
                let need = normalize(&requirement.text);
                !checks.iter().any(|check| covers(check, &need))
            }
        })
        .map(|requirement| requirement.text.clone())
        .collect()
}

/// The most accounting entries one `update_goal` call may carry.
///
/// It is a bound on the tool payload, not a statement about how much work a
/// user may ask for: the caller passes `max(requirement count, this floor)`,
/// so a legitimate long task can always account for every requirement it has.
pub(crate) const MIN_ACCOUNTING_ENTRIES: usize = 20;

/// Parse the optional `requirements` accounting array from `update_goal`.
///
/// `limit` exists so the cap can follow the requirement set: a fixed 20 made a
/// goal with 21 extracted requirements impossible to complete, because every
/// requirement needs its own account and the array could not hold them.
pub(crate) fn parse_accounting(
    arguments: &serde_json::Value,
    limit: usize,
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
    let limit = limit.max(MIN_ACCOUNTING_ENTRIES);
    if items.len() > limit {
        return Err(format!(
            "`requirements` may list at most {limit} entries; group related checks or account              for the requirements the user actually stated"
        ));
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
            // Nothing is owed for a prohibition: it constrains how the work is
            // done and is enforced by the tool/authority boundary, not by a
            // completion account. It cannot be "done" and must not be reported
            // as an unaccounted deliverable either.
            RequirementKind::Forbidden => {}
            RequirementKind::Tests => {
                let Some(fresh) =
                    facts.runs.iter().rev().find(|run| {
                        crate::executor::goal_verification::is_test_command(&run.label)
                    })
                else {
                    refusals.push(format!(
                        "the user explicitly requires the tests to pass, but no test command ran \
                         to completion in this task ({})",
                        crate::executor::goal_verification::test_command_guidance()
                    ));
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
                // Symmetric with the commit gate: a phrase the user listed as
                // several deliverables can be accounted by one entry naming
                // it or by one entry per part. Reading it only as the literal
                // joined phrase refused an account the same model was told to
                // write at commit time.
                let Some(accounts) = covering_accounts(accounting, &requirement.text) else {
                    refusals.push(unaccounted(requirement));
                    continue;
                };
                // An admitted gap outranks an unverified claim, which outranks
                // a plain done claim; one part is not done, so the phrase is
                // not done.
                if let Some(not_done) = accounts
                    .iter()
                    .find(|account| account.status == AccountStatus::NotDone)
                {
                    refusals.push(format!(
                        "`{}` is accounted not_done ({})",
                        requirement.text, not_done.evidence
                    ));
                } else if let Some(unverifiable) = accounts
                    .iter()
                    .find(|account| account.status == AccountStatus::Unverifiable)
                {
                    warnings.push(format!(
                        "requirement `{}` is explicitly unverifiable ({}); the completion carries \
                         an unverified boundary",
                        requirement.text, unverifiable.evidence
                    ));
                } else {
                    warnings.push(format!(
                        "requirement `{}` is reported done but has no mechanical check; the \
                         completion rests on the model's account, not runtime evidence",
                        requirement.text
                    ));
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
    // The requirement and the model's account may spell the same artifact at
    // different lengths: the user names `internal/report/aggregate.go` once
    // and `aggregate.go` later, and the model accounts for the shorter form
    // while its evidence names the full path. Naming it in either field is
    // still accounting for it; the artifact check below remains mechanical.
    accounts.iter().find(|account| {
        let accounted = normalize(&account.requirement);
        if need.chars().count() < 2 {
            // Containment cannot match a one-character requirement: every
            // account contains it. Exact identity is the only honest match,
            // and without it the requirement could never be accounted for.
            return accounted == need;
        }
        covers(&accounted, &need)
            || covers(&normalize(&account.evidence), &need)
            || strip_counted_summary(&need).is_some_and(|stem| {
                covers(&accounted, stem) || covers(&normalize(&account.evidence), stem)
            })
    })
}

/// Whether an acceptance check names the requirement. The requirement must
/// appear in the check, never the reverse: a narrower check does not cover the
/// user's deliverable.
fn covers(check: &str, requirement: &str) -> bool {
    requirement.chars().count() >= 2 && check.contains(requirement)
}

/// Generic category nouns a user appends after a counted list
/// (`…、内容权限校验三个功能`). The count and the category describe the list; they
/// are not part of the last deliverable's name, and a check that names the
/// deliverable does cover it.
const COUNTED_CATEGORY_NOUNS: &[&str] = &[
    "功能", "能力", "模块", "需求", "任务", "页面", "接口", "业务", "流程", "系统",
];

/// Classifiers that turn a number into a counted phrase (`三个`, `2项`, `三种`).
const COUNT_CLASSIFIERS: &[&str] = &["个", "项", "条", "种", "类", "块"];

const CJK_NUMERALS: &[char] = &[
    '一', '二', '三', '四', '五', '六', '七', '八', '九', '十', '百', '千', '万', '两', '零',
];

/// The deliverable with a trailing counted summary removed.
///
/// `内容权限校验三个功能` names `内容权限校验`: the trailing `三个功能` counts the list
/// it ends, and a check (`内容权限校验覆盖作者与管理员`) that names the deliverable
/// must not be refused because it does not repeat the count. The rule is
/// structural — a trailing generic category noun, prefixed by a numeral and a
/// classifier — and it only ever REMOVES a suffix, so it can only add ways to
/// cover a requirement, never weaken the requirement the user stated.
///
/// A short stem is refused: `支持三种登录方式` keeps its text because `方式` is not
/// a counted category noun, and because a two-character stem (`支持`) would let
/// any check containing the verb stand in for the deliverable.
fn strip_counted_summary(text: &str) -> Option<&str> {
    let text = text.trim();
    for noun in COUNTED_CATEGORY_NOUNS {
        let Some(head) = text.strip_suffix(noun) else {
            continue;
        };
        let head = head.trim_end();
        let Some((classifier_at, classifier)) = head.char_indices().next_back() else {
            continue;
        };
        if !COUNT_CLASSIFIERS
            .iter()
            .any(|candidate| *candidate == classifier.to_string())
        {
            continue;
        }
        let before_classifier = &head[..classifier_at];
        let digits_len: usize = before_classifier
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit() || CJK_NUMERALS.contains(c))
            .map(char::len_utf8)
            .sum();
        if digits_len == 0 {
            continue;
        }
        let stem = &before_classifier[..before_classifier.len() - digits_len];
        let stem =
            stem.trim_end_matches(|c: char| c.is_whitespace() || "，,、。；;：:".contains(c));
        if stem.chars().count() >= 4 {
            return Some(stem);
        }
    }
    None
}

/// The parts a user's deliverable phrase can be named by separately.
///
/// One extracted item can carry the rest of a list ("文章、讨论和评论界面"
/// extracts `文章` and `讨论和评论界面`). A model that names each part has
/// covered the phrase; demanding the literal concatenation refused honest
/// contracts.
///
/// The reading is structural, and it can only ADD a way to cover the phrase:
/// the literal phrase is always accepted too, and every part is a substring of
/// it, so a check that names the phrase also names every part. What the split
/// must not do is invent parts inside one word:
///
/// - explicit list punctuation (`、` `，` `,` `；` `;`) always separates;
/// - a conjunction (`和` `与` `及`) separates only in coordinator position —
///   between two parts that already have at least two characters on each side,
///   so `和平协议`, `及时通知` and `总和与均值` keep their first character;
/// - `/` and `+` never separate: inside a user-named deliverable they are a
///   technical token (`CI/CD`, `TCP/IP`, `HTTP/2`), and splitting them made
///   `ci` and `cd` into requirements that any unrelated check could satisfy.
fn named_parts(need: &str) -> Vec<String> {
    let chars: Vec<char> = need.chars().collect();
    let separator = |index: usize| {
        let ch = chars[index];
        if matches!(ch, '、' | '，' | ',' | '；' | ';') {
            return true;
        }
        if !matches!(ch, '和' | '与' | '及') {
            return false;
        }
        // Coordinator position: at least two characters of a part on each
        // side since the previous separator. A conjunction that opens a word
        // (`和平`), follows a single character (`总和`), or ends the phrase is
        // part of that word, not a list.
        let left = chars[..index]
            .iter()
            .rev()
            .take_while(|c| !matches!(**c, '、' | '，' | ',' | '；' | ';'))
            .count();
        let right = chars[index + 1..]
            .iter()
            .take_while(|c| !matches!(**c, '、' | '，' | ',' | '；' | ';'))
            .count();
        left >= 2 && right >= 2
    };
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for (index, ch) in chars.iter().enumerate() {
        if separator(index) {
            parts.push(std::mem::take(&mut current));
            continue;
        }
        current.push(*ch);
    }
    parts.push(current);
    parts
        .into_iter()
        .map(|part| part.trim().to_string())
        .filter(|part| part.chars().count() >= 2)
        .collect()
}

/// Whether checks cover a named deliverable: the phrase itself, or each of
/// its parts.
fn covers_named(checks: &[String], requirement: &str) -> bool {
    let need = normalize(requirement);
    let names = |wanted: &str| {
        checks.iter().any(|check| {
            covers(check, wanted)
                || strip_counted_summary(wanted).is_some_and(|stem| covers(check, stem))
        })
    };
    if names(&need) {
        return true;
    }
    let parts = named_parts(&need);
    parts.len() >= 2 && parts.iter().all(|part| names(part))
}

/// The completion accounts that cover one named deliverable, or `None` when
/// some part of the phrase was never accounted for.
///
/// This is the completion-side twin of [`covers_named`]: the runtime cannot
/// accept per-part acceptance checks at `commit_task` and then demand a single
/// joined accounting entry at `update_goal(complete)`.
fn covering_accounts<'a>(
    accounts: &'a [RequirementAccount],
    requirement: &str,
) -> Option<Vec<&'a RequirementAccount>> {
    let need = normalize(requirement);
    let names = |account: &RequirementAccount, wanted: &str| {
        let label = normalize(&account.requirement);
        // A single letter in prose could be an article or pronoun. Require
        // an exact label or explicit identifier syntax in the label itself.
        if wanted.chars().count() == 1 {
            let raw = &account.requirement;
            let name = clean_token(requirement);
            return label == wanted
                || raw.contains(&format!("`{name}`"))
                || raw.match_indices('=').any(|(index, _)| {
                    raw[..index]
                        .trim_end()
                        .rsplit(|c: char| !c.is_alphanumeric() && c != '_')
                        .next()
                        == Some(name.as_str())
                });
        }
        let evidence = normalize(&account.evidence);
        covers(&label, wanted)
            || covers(&evidence, wanted)
            || strip_counted_summary(wanted)
                .is_some_and(|stem| covers(&label, stem) || covers(&evidence, stem))
    };
    let direct: Vec<&RequirementAccount> = accounts
        .iter()
        .filter(|account| names(account, &need))
        .collect();
    if !direct.is_empty() {
        return Some(direct);
    }
    let parts = named_parts(&need);
    if parts.len() < 2 {
        return None;
    }
    let mut covering: Vec<&RequirementAccount> = Vec::with_capacity(parts.len());
    for part in &parts {
        let account = accounts.iter().find(|account| names(account, part))?;
        if !covering
            .iter()
            .any(|existing| std::ptr::eq(*existing, account))
        {
            covering.push(account);
        }
    }
    Some(covering)
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
        collect_constraint_clauses(segment, source, out);
    }
}

/// Prohibitions written as a standalone constraint clause, not as a bullet
/// under a heading.
///
/// A user may state `约束：禁止新增第三方依赖，禁止修改 … 以外的文件` in one
/// prose sentence. The requirement extractor only enumerated list items, so
/// those constraints were visible in the objective but absent from the
/// structured scope the model is shown at `commit_task`. They stay
/// `Forbidden` (no deliverable, no acceptance check, no completion account):
/// this records the constraint, never a gate over it.
fn collect_constraint_clauses(
    text: &str,
    source: &RequirementSource,
    out: &mut Vec<(String, RequirementKind, RequirementSource)>,
) {
    for sentence in text.split(['\n', '。', '；', ';']) {
        let sentence = sentence.trim();
        let sentence = strip_constraint_label(sentence);
        if !starts_with_prohibition(sentence) {
            continue;
        }
        // A comma inside a constraint clause separates independent
        // prohibitions. Splitting only when the next part ALSO opens with a
        // prohibition keeps `禁止修改 a 和 b` whole instead of manufacturing a
        // second requirement out of its object.
        for part in sentence.split(['，', ',']) {
            let part = part.trim();
            if !starts_with_prohibition(part) {
                continue;
            }
            let Some(text) = clean_item(part) else {
                continue;
            };
            out.push((text, RequirementKind::Forbidden, source.clone()));
        }
    }
}

fn strip_constraint_label(text: &str) -> &str {
    let trimmed = text.trim_start();
    for label in [
        "约束：",
        "约束:",
        "禁止事项：",
        "禁止事项:",
        "Constraints:",
        "Constraints：",
        "Constraint:",
        "Constraint：",
    ] {
        if let Some(rest) = trimmed.strip_prefix(label) {
            return rest.trim_start();
        }
    }
    trimmed
}

fn starts_with_prohibition(text: &str) -> bool {
    let text = strip_list_marker(text);
    let text = strip_constraint_label(&text);
    let lower = text.to_lowercase();
    [
        "禁止",
        "严禁",
        "不得",
        "不允许",
        "不要",
        "不能",
        "别",
        "must not",
        "may not",
        "do not",
        "don't",
        "never",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn split_segments(text: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        // A hard-wrapped sentence is one clause: the line break is whitespace,
        // not a boundary. A lowercase line continues the sentence above it, so
        // a permission survives the wrap ("you may add fixtures under /
        // a `testdata/` directory") instead of turning the paths it lists into
        // demands the user never made. Only a line above a permission can
        // absorb a continuation, and a paragraph break, a new sentence or a
        // list item still starts its own clause.
        let continues = !current.is_empty()
            && line.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && has_marker(&current, PERMISSION_ASCII, PERMISSION_CJK);
        if continues {
            current.push(' ');
            current.push_str(line.trim());
            continue;
        }
        if !current.is_empty() {
            segments.extend(
                current
                    .split(|c: char| CLAUSE_BREAKS.contains(&c))
                    .map(str::to_string),
            );
        }
        current.clear();
        current.push_str(line);
    }
    if !current.is_empty() {
        segments.extend(
            current
                .split(|c: char| CLAUSE_BREAKS.contains(&c))
                .map(str::to_string),
        );
    }
    segments
}

fn collect_segment(
    segment: &str,
    source: &RequirementSource,
    out: &mut Vec<(String, RequirementKind, RequirementSource)>,
) {
    let segment = strip_fenced_blocks(segment);
    collect_backticks(&segment, source, out);
    collect_paths(&segment, source, out);
    // An ATX heading (`## 一、项目目标`) is document structure. Its title names
    // a section, not a deliverable the user enumerated. Splitting it at `、`
    // produced requirements like `## 一`, which no honest acceptance list can
    // cover. Paths and inline-code tokens above still count when a heading
    // names a real artifact.
    if !is_markdown_heading(&segment) {
        collect_enumerations(&segment, source, out);
    }
}

/// An ATX Markdown heading: one to six `#` followed by whitespace.
fn is_markdown_heading(segment: &str) -> bool {
    let trimmed = segment.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes)
        && trimmed[hashes..]
            .chars()
            .next()
            .is_some_and(char::is_whitespace)
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
        // An illustration is not a deliverable. Skip the whole span so its
        // tokens cannot be re-added by the bare-path scan either.
        if example_context(segment, span) {
            continue;
        }
        let mut added_path = false;
        for token in span.split_whitespace() {
            let token = clean_token(token);
            if !path_like(&token) {
                continue;
            }
            added_path = true;
            // A permitted path is not a deliverable. Marking it seen also
            // keeps the surrounding backtick span from becoming a named one.
            if permission_only(segment, &token) {
                continue;
            }
            out.push((token.clone(), path_kind(segment, &token), source.clone()));
        }
        if added_path || is_test_phrase(span) || permission_only(segment, span) {
            continue;
        }
        // A backticked command the user asked to run is mechanical evidence,
        // not a named deliverable. The standard test runner is already the
        // Tests requirement; a non-test command must have run.
        if crate::executor::goal_verification::is_runner_named_test_command(span) {
            continue;
        }
        if command_text(span) {
            out.push((span.to_string(), RequirementKind::Command, source.clone()));
            continue;
        }
        if named_span(span) {
            out.push((span.to_string(), RequirementKind::Named, source.clone()));
        }
    }
}

/// Whether a backticked span names a deliverable rather than quoting code.
///
/// Users quote expressions and conditions to explain the problem (`Valid =
/// false`), and those are facts about the code, not items they asked for. A
/// deliverable name is one token without operators; anything else stays out
/// of the requirement set, consistent with under-extraction.
fn named_span(span: &str) -> bool {
    let mut words = span.split_whitespace();
    let Some(word) = words.next() else {
        return false;
    };
    if words.next().is_some() {
        return false;
    }
    !word.contains(['=', '<', '>', '+', '(', ')'])
}

fn collect_paths(
    segment: &str,
    source: &RequirementSource,
    out: &mut Vec<(String, RequirementKind, RequirementSource)>,
) {
    for raw in segment.split(|c: char| TOKEN_BREAKS.contains(&c)) {
        let token = clean_token(raw);
        // A Windows interpreter flag (`cmd /c verify.cmd`) belongs to the
        // command, not to the deliverables the user named.
        if interpreter_flag(&token) {
            continue;
        }
        if !path_like(&token) || looks_like_url(&token) || example_context(segment, &token) {
            continue;
        }
        // A path mentioned in a prohibition is a constraint, not a deliverable.
        if prohibition_around(segment, &token) {
            continue;
        }
        let clause = clause_around(segment, &token);
        let kind = if executable_path(&token) && has_any(clause, RUN_VERBS) {
            RequirementKind::Command
        } else {
            path_kind(segment, &token)
        };
        if matches!(kind, RequirementKind::Artifact { .. }) && permission_only(segment, &token) {
            continue;
        }
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
    // An ideographic-comma list is an enumeration only when the clause that
    // holds it is itself a delivery instruction. Splitting every `、` turned
    // descriptive phrases into requirements: `把文章、讨论、评论三条业务链路做成
    // 闭环` yielded the fragments `文章`, `讨论`, and `评论三条业务链路做成闭环`,
    // none of which the user asked for as separate deliverables — and the
    // fragments could never be covered, so `commit_task` refused an honest
    // contract. The gate is STRUCTURAL (does this clause start with an
    // instruction to deliver?) not a word list about what the list "means".
    let mut enumerated_ideographically = false;
    for clause in enumerated.split([',', '，']) {
        if !starts_with_delivery_instruction(clause) {
            continue;
        }
        let ideographic: Vec<&str> = clause.split('、').collect();
        if ideographic.len() < 2 {
            continue;
        }
        enumerated_ideographically = true;
        items.extend(ideographic.iter().map(|item| (*item).to_string()));
    }
    if let Some((head, tail)) = segment.rsplit_once(['：', ':'])
        && !enumerated_ideographically
        && has_any(head, DELIVERY_VERBS)
    {
        // Chinese lists use `、`, not commas. A Chinese comma after a colon
        // is a sequence ("先创建 A，再运行 B，完成前不要结束"), not an
        // enumeration; reading it as one manufactures requirements out of
        // instructions.
        let comma_items: Vec<&str> = tail.split([',', '，']).collect();
        if (2..=6).contains(&comma_items.len())
            && comma_items.iter().all(|item| {
                let item = item.trim();
                item.is_ascii()
                    && item.chars().count() <= 40
                    && item.split_whitespace().count() <= 4
                    && !item.contains(['(', ')', '（', '）'])
                    && !item.chars().next().is_some_and(|c| c.is_ascii_digit())
            })
        {
            items.extend(comma_items.iter().map(|item| (*item).to_string()));
        }
    }

    for item in items {
        // A prohibition is a constraint the user stated, not work to deliver.
        // It keeps its own kind so neither `commit_task` coverage nor the
        // completion account can ever treat it as a finished deliverable.
        let forbidden = is_prohibition(&item);
        let Some(text) = clean_item(&item) else {
            continue;
        };
        if path_like(&text) || contains_path(&text) || text.contains('`') {
            continue;
        }
        if is_test_phrase(&text)
            || crate::executor::goal_verification::is_runner_named_test_command(&text)
            || command_text(&text)
        {
            continue;
        }
        let kind = if forbidden {
            RequirementKind::Forbidden
        } else {
            RequirementKind::Named
        };
        out.push((text, kind, source.clone()));
    }
}

/// Whether a clause opens with an instruction to produce something.
///
/// The clause may start with a list marker (`1.`, `-`), then an obligation
/// word (`必须`), then the delivery verb. Anything else — a background clause,
/// a permission clause, a continuation such as `包括…` — is not an
/// enumeration of deliverables.
fn starts_with_delivery_instruction(clause: &str) -> bool {
    let text = strip_list_marker(clause);
    let after_obligation = strip_obligation_prefix(&text);
    DELIVERY_VERBS
        .iter()
        .any(|verb| after_obligation.starts_with(verb))
}

/// Remove one leading obligation word (`必须`, `需要`, `must`) if present.
fn strip_obligation_prefix(text: &str) -> &str {
    let trimmed = text.trim_start();
    for marker in OBLIGATION_CJK.iter().chain(OBLIGATION_ASCII) {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return rest.trim_start_matches(|c: char| c.is_whitespace() || c == '：' || c == ':');
        }
    }
    trimmed
}

/// Remove a leading list marker (`-`, `*`, `1.`, `(1)`, `一、`) from a clause.
fn strip_list_marker(clause: &str) -> String {
    bullet_item(clause).unwrap_or_else(|| clause.trim_start().to_string())
}

/// Whether the user is forbidding something rather than asking for it.
///
/// Reuses the same negation vocabulary that already guards test demands and
/// path removal, plus the explicit prohibition words that have no other
/// reading. A reminder (`不要忘记…`) is a demand and is excluded first.
fn is_prohibition(text: &str) -> bool {
    if has_any(text, WAIVER_GUARDS) {
        return false;
    }
    has_any(
        text,
        &["不得", "严禁", "不允许", "must not", "may not", "mustn't"],
    ) || segment_is_negated(text)
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
        RequirementKind::Forbidden => 5,
    }
}

/// Classify one path token by the verb that governs it.
///
/// The nearest governing verb before the token decides, not the sentence: a
/// single clause can delete one path and create another (`delete `old.txt`,
/// create `new.txt``), and a whole-sentence scan would apply the deletion verb
/// to both. Both the backticked and the bare-path scan use this one function,
/// so one path yields one requirement.
fn path_kind(segment: &str, token: &str) -> RequirementKind {
    if removal_expected(segment, token) {
        return RequirementKind::Artifact { removed: true };
    }
    // A command is judged over its clause, not its nearest verb: `go build
    // ./...` names the run first and the executable afterwards.
    if executable_path(token) && has_any(clause_around(segment, token), RUN_VERBS) {
        return RequirementKind::Command;
    }
    RequirementKind::Artifact { removed: false }
}

/// Whether the user asked for `token` to be absent.
///
/// The nearest verb wins among removal, delivery and negation words, so
/// `delete `a` and create `b`` classifies each path by its own verb and
/// `do not delete `a`` is not a removal requirement.
fn removal_expected(segment: &str, token: &str) -> bool {
    let Some(at) = last_match_end(segment, token) else {
        return false;
    };
    let prefix = &segment[..at];
    let nearest = REMOVAL_VERBS
        .iter()
        .map(|needle| (needle, true))
        .chain(DELIVERY_VERBS.iter().map(|needle| (needle, false)))
        .chain(NEGATIONS.iter().map(|needle| (needle, false)))
        .filter_map(|(needle, removal)| last_match_end(prefix, needle).map(|end| (end, removal)))
        .max_by_key(|(end, _)| *end);
    matches!(nearest, Some((_, true)))
}

/// Byte offset of the end of the last case-insensitive occurrence of `needle`
/// in `haystack`. ASCII case folding only: the verb lists are ASCII or CJK, and
/// folding per char keeps offsets valid for both.
fn last_match_end(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let mut best = None;
    for (start, _) in haystack.char_indices() {
        if let Some(candidate) = haystack[start..].get(..needle.len())
            && candidate.eq_ignore_ascii_case(needle)
        {
            best = Some(start + candidate.len());
        }
    }
    best
}

const PERMISSION_ASCII: &[&str] = &["may", "allowed", "optional"];
const PERMISSION_CJK: &[&str] = &["也可以", "也可", "可以", "允许"];
const OBLIGATION_ASCII: &[&str] = &["must", "required"];
const OBLIGATION_CJK: &[&str] = &["必须", "需要", "应当", "应该"];

/// A path the user permitted, rather than one the user required.
///
/// The permission covers later comma-separated items in the same sentence
/// ("you may add A, add B, and update `docs/`"). It stops at the sentence,
/// and a clause that states its own obligation ("but you must update
/// `README.md`") keeps that clause's paths. An obligation also covers the
/// items listed after it until another permission starts.
/// Markers that introduce an illustration rather than a deliverable.
const EXAMPLE_MARKERS: &[&str] = &[
    "e.g.",
    "e.g",
    "for example",
    "such as",
    "例如",
    "比如",
    "譬如",
];

/// Whether `token` is the immediate illustration of an example marker.
///
/// ``e.g. `p01.txt` `` names the shape of a file, not one the user wants
/// produced. Only the token directly introduced by the marker is skipped, so a
/// real deliverable later in the same sentence is still owed.
fn example_context(segment: &str, token: &str) -> bool {
    let Some(end) = last_match_end(segment, token) else {
        return false;
    };
    // `last_match_end` is the END of the token; the context between the
    // marker and the token starts before it.
    let start = end.saturating_sub(token.len());
    let prefix = &segment[..start];
    EXAMPLE_MARKERS.iter().any(|marker| {
        last_match_end(prefix, marker).is_some_and(|end| {
            prefix[end..].chars().all(|c| {
                c.is_whitespace() || "`'\"“”‘’。，,、；;：:!?！？()（）[]【】{}<>".contains(c)
            })
        })
    })
}

fn permission_only(segment: &str, token: &str) -> bool {
    if token.is_empty() || !segment.contains(token) {
        return false;
    }
    let sentence = sentence_around(segment, token);
    let mut permitted = false;
    let mut obliged = false;
    for clause in sentence.split([',', '，']) {
        let local_obligation = has_marker(clause, OBLIGATION_ASCII, OBLIGATION_CJK);
        let local_permission = has_marker(clause, PERMISSION_ASCII, PERMISSION_CJK);
        if clause.contains(token) {
            if local_obligation {
                return false;
            }
            return local_permission || (permitted && !obliged);
        }
        if local_obligation {
            obliged = true;
            permitted = false;
        }
        if local_permission {
            permitted = true;
            obliged = false;
        }
    }
    false
}

/// The sentence that contains `token`. A period ends it only when the next
/// word starts a new sentence, so `README.md` and `./...` stay intact.
fn sentence_around<'a>(segment: &'a str, token: &str) -> &'a str {
    let Some(at) = segment.find(token) else {
        return segment;
    };
    let mut start = 0;
    let mut index = 0;
    while index < at {
        let ch = segment[index..].chars().next().unwrap();
        if sentence_boundary(segment, index) {
            start = index + ch.len_utf8();
        }
        index += ch.len_utf8();
    }
    let mut end = segment.len();
    let mut index = at + token.len();
    while index < segment.len() {
        if sentence_boundary(segment, index) {
            end = index;
            break;
        }
        index += segment[index..].chars().next().unwrap().len_utf8();
    }
    &segment[start..end]
}

fn sentence_boundary(text: &str, index: usize) -> bool {
    let rest = &text[index..];
    let Some(ch) = rest.chars().next() else {
        return false;
    };
    match ch {
        '\n' | '。' | '；' | ';' | '!' | '?' | '！' | '？' => true,
        '.' => {
            let after = rest[ch.len_utf8()..].trim_start();
            match after.chars().next() {
                None => true,
                Some(next) => next.is_uppercase(),
            }
        }
        _ => false,
    }
}

fn has_marker(text: &str, ascii: &[&str], cjk: &[&str]) -> bool {
    let lower = text.to_lowercase();
    if ascii.iter().any(|word| ascii_word(&lower, word)) {
        return true;
    }
    cjk.iter().any(|marker| cjk_marker(text, marker))
}

fn ascii_word(lower: &str, word: &str) -> bool {
    lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|part| part == word)
}

/// CJK markers are substrings. A preceding `不` negates the marker
/// (`不可以`, `不需要`), so that occurrence does not count.
fn cjk_marker(text: &str, marker: &str) -> bool {
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        if !rest[..at].ends_with('不') {
            return true;
        }
        let next = at + marker.len();
        if next >= rest.len() {
            break;
        }
        rest = &rest[next..];
    }
    false
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

/// Whether the clause asks something rather than instructs.
///
/// A question mark is one form; Chinese and English also ask without one
/// ("测试通过了吗", "刚才改了哪些文件", "What changed?"). This is a *shape*
/// test only: it says the clause needs its intent read, not that it is
/// irrelevant.
fn question_form(segment: &str) -> bool {
    let trimmed = trim_trailing_punctuation(segment);
    let lower = trimmed.to_ascii_lowercase();
    trimmed.ends_with('?')
        || trimmed.ends_with('？')
        || trimmed.ends_with('吗')
        || trimmed.ends_with('么')
        || trimmed.ends_with('呢')
        || has_any(trimmed, QUESTION_WORDS_CJK)
        || QUESTION_WORDS_ASCII
            .iter()
            .any(|word| lower.starts_with(word) || lower.contains(&format!(" {word}")))
}

fn trim_trailing_punctuation(text: &str) -> &str {
    text.trim()
        .trim_end_matches(['"', '\'', '”', '’', '）', ')', ']', '】'])
        .trim_end()
}

/// Question form, minus the words that state the question's kind.
const QUESTION_WORDS_CJK: &[&str] = &[
    "是否",
    "有没有",
    "为什么",
    "怎么",
    "哪些",
    "哪个",
    "哪里",
    "哪儿",
    "多少",
    "什么",
];

const QUESTION_WORDS_ASCII: &[&str] = &[
    "what", "which", "who", "where", "when", "why", "how", "did", "does", "do ", "have", "has",
    "was", "were", "is ", "are ", "can ", "could ", "should ", "would ", "will ",
];

/// Phrases that ask for advice about work instead of asking for the work.
const CONSULTATION_CJK: &[&str] = &[
    "如果",
    "假如",
    "假设",
    "要是",
    "若是",
    "万一",
    "有什么影响",
    "有何影响",
    "会有什么后果",
    "有什么风险",
    "有什么问题",
    "是否值得",
    "值不值得",
    "有没有必要",
    "是否有必要",
    "需不需要",
    "要不要",
    "该不该",
    "是否应该",
];

const CONSULTATION_ASCII: &[&str] = &[
    "what if",
    "what would happen",
    "would it",
    "is it necessary",
    "is it worth",
    "do we need",
    "should we",
    "how would",
    "could we instead",
    "what are the risks",
    "any impact",
];

/// Phrases that ask about the state of work that already exists.
const STATUS_CJK: &[&str] = &[
    "了吗",
    "了么",
    "了没",
    "过吗",
    "好了没",
    "做完没",
    "哪些",
    "哪里",
    "哪儿",
    "怎么样",
    "进度",
    "情况如何",
    "什么情况",
    "当前状态",
    "现状",
    "跑过",
    "测试过",
    "是否已经",
    "是否已",
    "有没有完成",
    "有没有通过",
    "有没有跑",
    "是否完成",
    "是否通过",
];

const STATUS_ASCII: &[&str] = &[
    "passed",
    "failed so far",
    "changed",
    "modified",
    "done yet",
    "finished yet",
    "so far",
    "status",
    "progress",
    "test result",
    "did the tests",
    "tests pass",
    "tests passed",
    "test pass",
    "all tests pass",
    "did you",
    "have you",
];

/// Whether the clause is one the user wants work done for.
///
/// The default for an unclassified question is `true`: dropping a clause only
/// because of its punctuation is the mistake this guards. A clause is skipped
/// only when it demonstrably asks for information, discusses a hypothetical,
/// or changes the execution authorization — and an authorization change is the
/// lifecycle's fact, not a deliverable.
fn carries_scope(clause: &str) -> bool {
    let clause = clause.trim();
    if clause.is_empty() {
        return false;
    }
    if is_execution_authorization(clause) {
        return false;
    }
    let lower = clause.to_ascii_lowercase();
    if has_any(clause, CONSULTATION_CJK)
        || CONSULTATION_ASCII.iter().any(|word| lower.contains(word))
    {
        return false;
    }
    !is_status_query(clause)
}

fn is_status_query(clause: &str) -> bool {
    let lower = clause.to_ascii_lowercase();
    has_any(clause, STATUS_CJK) || STATUS_ASCII.iter().any(|word| lower.contains(word))
}

/// A clause that changes the user's execution authorization instead of naming
/// work: pausing, releasing, or waiving a stage. Those are read by
/// [`leveler_lifecycle`], never as acceptance scope.
fn is_execution_authorization(clause: &str) -> bool {
    leveler_lifecycle::extract_user_pause(clause).is_some()
        || leveler_lifecycle::authorizes_continuation(clause)
        || (is_waiver(clause)
            && has_any(
                clause,
                &["阶段", "步骤", "stage", "phase", "step", "继续", "continue"],
            ))
}

/// The parts of a question-form message that ask for work.
///
/// Splitting follows the clauses the person wrote (commas, sentence ends, and
/// question marks) so a mixed message keeps both halves: "测试通过了吗？能不能
/// 顺便把 X 也补上？" contains a status query and a request, and only the
/// request names work.
fn request_clauses(question: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    let mut current = String::new();
    for c in question.chars() {
        if matches!(
            c,
            '，' | ',' | '。' | '；' | ';' | '？' | '?' | '！' | '!' | '\n'
        ) {
            let clause = current.trim();
            if carries_scope(clause) {
                clauses.push(clause.to_string());
            }
            current.clear();
            continue;
        }
        current.push(c);
    }
    let clause = current.trim();
    if carries_scope(clause) {
        clauses.push(clause.to_string());
    }
    clauses
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

/// Words that ask for verification of something rather than name a deliverable.
const VERIFY_WORDS: &[&str] = &["测试", "实测", "验证", "检验", "检查", "test", "verify"];

/// The breadth quantifiers that introduce the subject of such a demand.
const SUBJECT_QUANTIFIERS: &[&str] = &[
    "全部",
    "所有",
    "每个",
    "每一个",
    "完整",
    "整个",
    "all",
    "every",
];

/// Characters that end the noun a quantifier introduces. They are particles,
/// adverbs and verbs — none of them can continue a noun phrase the user means
/// the runtime to cover.
const SUBJECT_STOP: &[char] = &[
    '的', '地', '得', '都', '也', '再', '又', '要', '需', '必', '先', '后', '上', '下', '过', '一',
    '遍', '真', '实', '全', '每', '和', '与', '及', '并', '在', '把', '对', '给', '请', '逐', '分',
    '从', '到', '里', '中', '吗', '呢', '了', '么',
];

/// The subject of a verification demand the runtime cannot enumerate.
///
/// `把全部功能真实地测试下` asks for verification of a SET. No file, no command
/// and no suite names it, so the runtime can never prove it — and the project's
/// own suite passing is not that proof. The runtime's only honest options are
/// to drop the demand (silently narrowing the user's scope) or to keep its
/// subject visible. It keeps the subject: the acceptance list must name it, the
/// completion account must cover it, and the record says the coverage is the
/// model's account, not runtime evidence.
///
/// The reading is structural and project-independent: a verification word and a
/// breadth quantifier in the same clause, followed by the noun the quantifier
/// counts. It never reads business meaning, never names a product and never
/// decides how much testing is enough.
/// Clauses of one segment, split on the punctuation that separates them.
///
/// Deliberately not [`request_clauses`]: that filter answers "does this clause
/// name work the user wants", and a clause that asks for verification names
/// nothing the extractor recognises as work — which is exactly the clause this
/// reader exists for.
fn split_clauses(segment: &str) -> Vec<&str> {
    segment
        .split(['，', ',', '。', '；', ';', '\n'])
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .collect()
}

fn verification_subject(segment: &str) -> Option<String> {
    // A clause is the unit: `把全部功能测试下，重点关注 MCP 有没有问题` states a
    // demand and then asks a question, and only the question is not a demand.
    for clause in split_clauses(segment) {
        if !has_any(&clause.to_lowercase(), VERIFY_WORDS) {
            continue;
        }
        // A waiver removes the demand, and a question is not a demand.
        if is_waiver(clause) || segment_is_negated(clause) || question_form(clause) {
            continue;
        }
        let lower = clause.to_lowercase();
        for quantifier in SUBJECT_QUANTIFIERS {
            let Some(at) = lower.find(quantifier) else {
                continue;
            };
            let rest = &clause[at + quantifier.len()..];
            let noun: String = rest
                .chars()
                .take_while(|c| {
                    !SUBJECT_STOP.contains(c)
                        && !c.is_whitespace()
                        && !matches!(
                            c,
                            '，' | ',' | '。' | '；' | ';' | '：' | ':' | '、' | '！' | '!'
                        )
                })
                .take(8)
                .collect();
            // Two characters is the shortest noun that names something: a
            // one-character subject would be covered by almost any account.
            if noun.chars().count() >= 2 {
                return Some(format!("{quantifier}{noun}"));
            }
        }
    }
    None
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

/// A one-letter absolute path is indistinguishable from a Windows interpreter
/// flag (`/c`, `/k`), and under-extraction is preferred to a requirement the
/// user never named.
fn interpreter_flag(token: &str) -> bool {
    let mut chars = token.chars();
    chars.next() == Some('/')
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.next().is_none()
}

/// Whether a token is a `NAME=value` environment assignment.
///
/// The value may look like a path (`CONFIG=internal/config/probe.yaml`), but
/// the token names a VARIABLE handed to a command, not a file. The program that
/// reads it resolves the path — often against a subproject directory, not this
/// workspace — so demanding the literal `NAME=value` exist refused a real
/// completion whose work was done: a DevOrder goal could never satisfy
/// `CONFIG=internal/config/config.probe.local.yaml`, a file that exists one
/// directory down and was never a deliverable in the first place.
fn env_assignment(token: &str) -> bool {
    let Some((name, value)) = token.split_once('=') else {
        return false;
    };
    !value.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn path_like(token: &str) -> bool {
    let token = clean_token(token);
    if token.is_empty() || looks_like_url(&token) || token.contains("//") || glob_pattern(&token) {
        return false;
    }
    if env_assignment(&token) {
        return false;
    }
    // A separator between two tokens (`count=`/`total=`) is punctuation, not
    // a path. Every real artifact names something with a letter or digit.
    if !token.chars().any(|c| c.is_alphanumeric()) {
        return false;
    }
    // Explicit path syntax.
    if token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('/')
        || token.starts_with("~/")
        || token.starts_with('\\')
        || token.contains('\\')
        || token.ends_with('/')
        || (token.len() >= 2 && token.as_bytes()[1] == b':')
    {
        return true;
    }
    // A bare `a/b` is ambiguous: users write "put/get" and "read/write" as
    // alternatives, not paths. Only a slash token whose last segment is a
    // known file name is treated as an artifact. Directory paths without an
    // extension are not extracted (under-extraction, never a false demand).
    let name = token.rsplit('/').next().unwrap_or(&token);
    let Some((stem, extension)) = name.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty() && PATH_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
}

/// A package pattern or shell wildcard names a set of files, not one artifact.
/// Demanding that the literal pattern exist produces a refusal no completion
/// can satisfy.
fn glob_pattern(token: &str) -> bool {
    let name = token.rsplit(['/', '\\']).next().unwrap_or(token);
    name == "..." || name.contains(['*', '?', '['])
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
    // A list marker is presentation, not part of the deliverable. An item that
    // kept its `1.` prefix and also appeared without it would be extracted
    // twice, and the numbered spelling could never be covered.
    let raw = strip_list_marker(raw);
    let raw = raw.as_str();
    // Keep only the leading clause of an item: a following comma usually
    // starts a new instruction, not part of the deliverable's name.
    let head = raw
        .split(['，', ',', '。', '；', ';'])
        .next()
        .unwrap_or(raw);
    // A colon is NOT a clause boundary when it separates a short label from the
    // instruction that follows it: `阶段 2：在 app.py 增加 articles_by_author`
    // names the stage, and the deliverable is what the stage asks for. Splitting
    // first kept only the label (`阶段 2`) and demanded an acceptance check for a
    // section title that no honest list contains.
    let head = match head.split_once(['：', ':']) {
        // A short label in front of the instruction is a section name
        // (`阶段 2：在 app.py 增加 …`); the deliverable is what follows.
        Some((label, rest))
            if label.chars().count() <= 24
                && !has_any(label, DELIVERY_VERBS)
                && has_any(rest, DELIVERY_VERBS) =>
        {
            rest.trim()
        }
        // Otherwise a colon after a delivery verb introduces the details of the
        // deliverable itself (`实现内容权限校验：作者可编辑，管理员可下架`): the
        // deliverable is the part before the details.
        Some((before, _)) if has_any(before, DELIVERY_VERBS) => before.trim(),
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
    // A sentence period is presentation: the user's bullet ends with one and
    // the model quotes the same requirement without it. A path never ends in
    // a dot, so trimming a trailing one cannot merge two artifacts.
    text.trim_end_matches(['.', '/']).to_string()
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
            call_id: None,
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

    /// The one reading that decides whether a user message joins the goal's
    /// durable instruction sequence. Scope and authorization are two separate
    /// facts read from the same words, and neither is a keyword match.
    #[test]
    fn only_an_instruction_joins_the_goal_sequence() {
        let objective = "创建 stage-1.txt，最后运行 sh ./verify.sh。\
             阶段 1 完成后停下，等待我确认再执行阶段 2。";
        let recorded: Vec<String> = Vec::new();

        // Scope: a real deliverable, in statement or in question form.
        for text in [
            "顺便把 src/permissions.rs 的评论权限校验也补上。",
            "能不能顺便把 src/comment_acl.rs 也补上？",
        ] {
            assert!(
                changes_scope(objective, &recorded, text),
                "a new deliverable is a scope change: {text}"
            );
            assert!(is_goal_instruction(objective, &recorded, text));
        }
        // A waiver is the user's authority to shrink scope.
        assert!(changes_scope(
            objective,
            &recorded,
            "stage-2.txt 暂时不做。"
        ));
        // Authorization: neither adds or removes a requirement, but the
        // newest directive is what releases (or re-sets) the pause.
        // The sequence is read as a SIGNAL, so a repeat of the pause the
        // objective already states adds nothing, while a new directive on an
        // otherwise un-paused goal does.
        let open = "创建 stage-1.txt，最后运行 sh ./verify.sh。";
        for text in ["确认，继续阶段 2。", "先停在这里，等我确认。"] {
            assert!(
                is_goal_instruction(open, &recorded, text),
                "an authorization change is durable with its goal: {text}"
            );
        }
        assert!(
            !is_goal_instruction(objective, &recorded, "先停在这里，等我确认。"),
            "a repeated pause changes no authorization fact"
        );
        // Nothing at all: a status question, a consultation, a restatement.
        for text in [
            "刚才修改了哪些文件？测试通过了吗？现在进度怎么样？",
            "如果增加 src/permissions.rs 的校验，会有什么影响？",
            "阶段 2 可以先不做吗？",
            "创建 stage-1.txt，最后运行 sh ./verify.sh。阶段 1 完成后停下，等待我确认再执行阶段 2。",
        ] {
            assert!(
                !changes_scope(objective, &recorded, text),
                "no requirement changed: {text}"
            );
            assert!(
                !is_goal_instruction(objective, &recorded, text),
                "a query or a restatement is not recorded: {text}"
            );
        }
        // A repeated continuation carries no new authorization signal.
        assert!(!is_goal_instruction(
            objective,
            &["确认，继续阶段 2。".to_string()],
            "继续。"
        ));
    }

    /// A Chat message joins an owed goal only when it changes the WORK.
    ///
    /// The reported hazard: a side request that merely says "only answer, do
    /// not change any file" would otherwise be remembered as the GOAL's own
    /// prohibition, and later stop the work the user authorized.
    #[test]
    fn a_chat_message_joins_a_goal_only_when_it_changes_the_work() {
        let objective = "创建 stage-1.txt，最后运行 sh ./verify.sh。\
             阶段 1 完成后停下，等待我确认再执行阶段 2。";
        let recorded: Vec<String> = Vec::new();

        // Work added, in statement or question form, and work waived.
        for text in [
            "顺便把 stage-extra.txt 也补上，内容为一行 extra。",
            "能不能顺便把 src/comment_acl.rs 也补上？",
            "stage-2.txt 暂时不做。",
        ] {
            assert!(
                is_chat_goal_instruction(objective, &recorded, text),
                "a change to the work joins the goal: {text}"
            );
        }
        // A constraint about a side request is NOT the goal's own.
        for text in [
            "另外帮我看一下 README 有没有过时的说明，只回答，不要改任何文件。",
            "解释一下这个模块，不要改代码。",
            "刚才修改了哪些文件？测试通过了吗？",
        ] {
            assert!(
                !is_chat_goal_instruction(objective, &recorded, text),
                "a side request adds no work, so it stays out of the goal: {text}"
            );
        }
        // The same words ARE the goal's own when the user is addressing the
        // goal's window: a prohibition there binds the work.
        assert!(is_goal_instruction(
            objective,
            &recorded,
            "不要修改 src/payments.rs。"
        ));
        // Authorization is a separate fact and travels on both readings.
        assert!(is_chat_goal_instruction(
            objective,
            &recorded,
            "确认，继续阶段 2。"
        ));
    }

    /// The real DevOrder objective. `CONFIG=internal/config/probe.yaml` was
    /// extracted as an artifact, so the completion gate demanded that a path
    /// literally named `CONFIG=internal/config/config.probe.local.yaml` exist —
    /// a file that cannot exist and that the user never named. The goal was
    /// unsatisfiable and the run ended incomplete with the work done.
    #[test]
    fn an_environment_assignment_is_not_a_deliverable_path() {
        let requirements = extract(
            "本地启动后全面真实测试下。环境：用 CONFIG=internal/config/config.probe.local.yaml \
             RUN_DIR=tmp/run-agent-probe API_PORT=18080 MCP_PORT=18081 PORTAL_PORT=13010 启动，\
             数据库已隔离为 devorder_agent_probe_20261031；不要改业务代码，发现问题如实报告。",
            &[],
        );
        assert!(
            requirements
                .iter()
                .all(|requirement| !requirement.text.starts_with("CONFIG=")),
            "a variable assignment is not a path: {requirements:?}"
        );
        assert!(
            requirements
                .iter()
                .all(|requirement| !matches!(requirement.kind, RequirementKind::Artifact { .. })),
            "the objective names no deliverable file: {requirements:?}"
        );
    }

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
    fn an_interpreter_flag_is_not_a_deliverable() {
        let requirements = extract(
            "完成全部三个阶段：创建 stage-1.txt、stage-2.txt、stage-3.txt，\
             最后运行 cmd /c verify.cmd，全部做完再结束。",
            &[],
        );
        let found = texts(&requirements);
        assert!(
            !found.contains(&"/c"),
            "the interpreter flag must not become a requirement: {found:?}"
        );
        assert!(
            requirements.iter().any(|requirement| {
                requirement.text == "verify.cmd" && requirement.kind == RequirementKind::Command
            }),
            "the named script stays a command requirement: {requirements:?}"
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

    /// A Chinese conjunction inside one enumerated item names its parts. An
    /// acceptance that checks both parts separately has not dropped either.
    #[test]
    fn a_conjoined_deliverable_is_covered_by_checks_for_each_part() {
        let requirements = extract("实现文章、讨论和评论界面。", &[]);
        let joined = requirements
            .iter()
            .find(|requirement| requirement.text == "讨论和评论界面")
            .expect("the conjoined phrase is one named deliverable");
        let left = uncovered(
            std::slice::from_ref(joined),
            &[
                "文章列表与详情界面可访问".to_string(),
                "讨论与评论界面可访问".to_string(),
            ],
        );
        assert!(
            left.is_empty(),
            "each part is named by a concrete check: {left:?}"
        );

        let narrowed = uncovered(
            std::slice::from_ref(joined),
            &["文章列表与详情界面可访问".to_string()],
        );
        assert_eq!(
            narrowed,
            vec!["讨论和评论界面".to_string()],
            "a check that omits the comment thread still leaves the deliverable uncovered"
        );
    }

    /// The T8 shape: the user enumerates an acceptance criterion ending with a
    /// sentence period, and the model quotes the criterion back without the
    /// period. That is the same requirement accounted for, not a missing one.
    #[test]
    fn an_account_that_drops_a_trailing_period_still_covers_the_requirement() {
        let requirements = extract(
            "Acceptance criteria:\n\
             - `go test ./...` passes.\n\
             - The boundary rule is covered by a test.\n\
             - The command-line report reflects the rule end to end.\n",
            &[],
        );
        let cli = requirements
            .iter()
            .find(|requirement| requirement.text.contains("command-line report"))
            .expect("the enumerated acceptance criterion is a requirement");
        let accounting = vec![account(
            "The command-line report reflects the rule end to end",
            AccountStatus::Done,
            "cmd/telemetryd/main_test.go drives run() through the pipeline",
        )];
        let facts = ScopeFacts {
            requirements: std::slice::from_ref(cli),
            accounting: Some(&accounting),
            runs: &[],
            mutation_ops: 0,
            artifact: &|_| ArtifactState::Unknown,
        };
        assert!(
            matches!(decide(&facts), ScopeDecision::Accept { .. }),
            "the period is presentation, not a different requirement: {cli:?}"
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

    /// A request keeps its authority in question form.
    ///
    /// `能不能顺便把 X 也补上？` asks for work; dropping every `?`-terminated
    /// clause silently erased a delivery the user asked for, so the goal was
    /// accepted without the thing they had just requested.
    #[test]
    fn a_request_in_question_form_is_scope_authority() {
        let objective = "创建 stage-1.txt，最后运行 sh ./verify.sh。";
        let base = extract(objective, &[]);
        for (request, expected) in [
            (
                "能不能顺便把 src/permissions.rs 的评论权限校验也补上？",
                "src/permissions.rs",
            ),
            (
                "请再加一个 src/comments_delete.rs 接口，可以吗？",
                "src/comments_delete.rs",
            ),
            (
                "Can you also add src/comment_acl.rs and run the tests?",
                "src/comment_acl.rs",
            ),
            ("顺便把 docs/comment_acl.md 也补上？", "docs/comment_acl.md"),
        ] {
            let amended = extract(objective, &[request.to_string()]);
            let found = texts(&amended);
            assert!(
                found.len() > base.len(),
                "a question-form request still adds scope: {request} -> {found:?}"
            );
            assert!(
                found.contains(&expected),
                "the asked-for deliverable is in scope: {request} -> {found:?}"
            );
        }
    }

    /// A mixed message keeps its request and drops its status query.
    #[test]
    fn a_mixed_message_keeps_only_its_request() {
        let objective = "创建 stage-1.txt，最后运行 sh ./verify.sh。";
        let amended = extract(
            objective,
            &["测试通过了吗？能不能顺便把 src/permissions.rs 也补上？".to_string()],
        );
        let found = texts(&amended);
        assert!(
            found.contains(&"src/permissions.rs"),
            "the request half is scope authority: {found:?}"
        );
        assert!(
            !amended
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "the query half must not become a test demand: {amended:?}"
        );
    }

    /// Questions that only discuss the work carry no scope authority.
    #[test]
    fn a_consultation_or_status_question_adds_nothing() {
        let objective = "创建 stage-1.txt，最后运行 sh ./verify.sh。";
        let base = extract(objective, &[]);
        for question in [
            "如果增加 src/permissions.rs 的校验，会有什么影响？",
            "阶段 2 可以先不做吗？",
            "是否有必要重构 src/permissions.rs？",
            "What if we add src/permissions.rs?",
            "Is it necessary to refactor src/permissions.rs?",
            "评论权限校验做好了吗？",
            "刚才修改了哪些文件？",
            "测试通过了吗？",
            "现在进度怎么样？",
            "Which files changed?",
            "Did the tests pass?",
        ] {
            let asked = extract(objective, &[question.to_string()]);
            assert_eq!(
                texts(&asked),
                texts(&base),
                "a question about the work changes no requirement: {question}"
            );
        }
    }

    /// A prohibition stated as a question is still a constraint, and an
    /// authorization change is neither a constraint nor a deliverable.
    #[test]
    fn a_question_form_constraint_and_authorization_are_told_apart() {
        let objective = "创建 stage-1.txt，最后运行 sh ./verify.sh。";
        let forbidden = extract(
            objective,
            &["不要修改 src/payments.rs，可以吗？".to_string()],
        );
        assert!(
            forbidden.iter().any(|requirement| {
                requirement.kind == RequirementKind::Forbidden
                    && requirement.text.contains("src/payments.rs")
            }),
            "a question-form prohibition stays a constraint: {forbidden:?}"
        );
        assert!(
            !forbidden.iter().any(|requirement| {
                requirement.kind == RequirementKind::Artifact { removed: false }
                    && requirement.text.contains("src/payments.rs")
            }),
            "a prohibition is not a deliverable: {forbidden:?}"
        );
        let feature = extract(objective, &["不要实现支付功能，可以吗？".to_string()]);
        assert!(
            feature
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Forbidden),
            "a forbidden feature survives the question mark: {feature:?}"
        );
        for authorization in [
            "能不能先不要执行阶段 2？",
            "确认，继续阶段 2，可以吗？",
            "阶段 2 可以先不做吗？",
        ] {
            let amended = extract(objective, &[authorization.to_string()]);
            assert_eq!(
                texts(&amended),
                texts(&base_requirements(objective)),
                "an authorization change is not a scope item: {authorization}"
            );
        }
    }

    /// A clause that only *sounds* past-tense is still an instruction. The
    /// status vocabulary is deliberately about the work's state, not about
    /// every word that can appear in a question.
    #[test]
    fn an_instruction_phrased_as_a_question_is_still_an_instruction() {
        let objective = "创建 stage-1.txt，最后运行 sh ./verify.sh。";
        for (request, expected) in [
            (
                "刚才的写法也要补到 src/legacy.rs，可以吗？",
                "src/legacy.rs",
            ),
            ("上一次的导出能不能也接到 src/export.rs？", "src/export.rs"),
        ] {
            let amended = extract(objective, &[request.to_string()]);
            let found = texts(&amended);
            assert!(
                found.contains(&expected),
                "the request keeps its authority: {request} -> {found:?}"
            );
        }
        let asked = extract(
            objective,
            &["顺便问一句，能不能把测试也跑一遍？".to_string()],
        );
        assert!(
            asked
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "a test run asked as a question is still demanded: {asked:?}"
        );
        // A result query that names a test run still adds no test demand.
        for query in [
            "上次的改动有没有通过测试？",
            "测试有没有跑过？",
            "Have you finished the export work?",
        ] {
            let asked = extract(objective, &[query.to_string()]);
            assert!(
                !asked
                    .iter()
                    .any(|requirement| requirement.kind == RequirementKind::Tests),
                "a result query is not a test demand: {query} -> {asked:?}"
            );
        }
    }

    /// A question-form requirement is a real gate: the acceptance list has to
    /// cover it and the completion account cannot claim it done.
    #[test]
    fn a_question_form_requirement_is_accepted_and_gated_like_any_other() {
        let objective = "创建 stage-1.txt，最后运行 sh ./verify.sh。";
        let requirements = extract(
            objective,
            &["能不能顺便把 src/permissions.rs 也补上？".to_string()],
        );
        let missing = uncovered(&requirements, &["stage-1.txt exists".to_string()]);
        assert!(
            missing.iter().any(|text| text == "src/permissions.rs"),
            "a question-form ask must be covered by acceptance: {missing:?}"
        );
    }

    /// The real Dogfood objective. `把全部功能真实的测试下` asks to exercise the
    /// product; it does not ask that the project's own suite passes. The
    /// runtime read the breadth words as a `tests` requirement, told the model
    /// its single explicit deliverable was `tests`, and then accepted the
    /// completion because one package's test command had passed — while 19
    /// operator tools were returning a generic internal failure and had never
    /// been called. Both halves are refused here: no `tests` requirement, and
    /// the demand's subject stays visible as something the runtime cannot
    /// prove mechanically.
    #[test]
    fn an_open_ended_verification_demand_is_not_a_passing_suite() {
        let requirements = extract(
            "本地启动项目，然后把全部功能真实在的测试下，重点关注 MCP 有没有问题。",
            &[],
        );
        assert!(
            !requirements
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "testing the product is not a demand that the suite passes: {requirements:?}"
        );
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.text == "全部功能"),
            "the subject of the demand must stay visible: {requirements:?}"
        );
    }

    /// The other side of the same reading: a demand that the tests PASS, or a
    /// named test runner, is still a mechanical requirement.
    #[test]
    fn a_demand_that_the_tests_pass_is_still_a_tests_requirement() {
        for objective in [
            "确保全部测试通过",
            "把测试跑一遍",
            "跑 cargo test",
            "所有测试必须通过",
            "实现全部接口，并通过完整测试。",
        ] {
            let requirements = extract(objective, &[]);
            assert!(
                requirements
                    .iter()
                    .any(|requirement| requirement.kind == RequirementKind::Tests),
                "{objective} demands passing tests: {requirements:?}"
            );
        }
    }

    /// A subject the runtime cannot enumerate is honest scope with honest
    /// evidence: the acceptance list must name it, the completion account must
    /// cover it, and the record says the coverage rests on that account.
    #[test]
    fn an_unenumerable_verification_subject_is_accounted_and_not_proven() {
        let requirements = vec![req("全部功能", RequirementKind::Named)];
        assert_eq!(
            uncovered(&requirements, &["make up 成功".to_string()]),
            vec!["全部功能".to_string()],
            "an acceptance list that never names the subject does not cover it"
        );
        assert!(
            uncovered(
                &requirements,
                &["全部功能实测：门户页面、运营接口、MCP 工具".to_string()]
            )
            .is_empty()
        );
        let done = [account("全部功能", AccountStatus::Done, "实测了三个域")];
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&done),
            runs: &[],
            mutation_ops: 0,
            artifact: &|_| ArtifactState::Unknown,
        });
        match decision {
            ScopeDecision::Accept { warnings } => assert!(
                warnings.iter().any(|warning| warning.contains("全部功能")
                    && warning.contains("no mechanical check")),
                "coverage of an unenumerable subject is a boundary, not proof: {warnings:?}"
            ),
            other => panic!("expected an accepted completion with a boundary, got {other:?}"),
        }
        let missing = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: None,
            runs: &[],
            mutation_ops: 0,
            artifact: &|_| ArtifactState::Unknown,
        });
        assert!(
            matches!(missing, ScopeDecision::Refuse(_)),
            "a completion that never accounts for the subject is refused: {missing:?}"
        );
    }

    fn base_requirements(objective: &str) -> Vec<Requirement> {
        extract(objective, &[])
    }

    /// A status question in a paused goal is not a new requirement. Reading
    /// `测试通过了吗？` as a demand would change the acceptance the goal must
    /// still satisfy, which is exactly what a question must not do.
    #[test]
    fn a_status_question_amendment_changes_no_requirement() {
        let objective = "创建 stage-1.txt 和 stage-2.txt，最后运行 sh ./verify.sh。";
        let base = extract(objective, &[]);
        let asked = extract(
            objective,
            &[
                "刚才阶段 1 修改了哪些文件？测试通过了吗？".to_string(),
                "现在进度怎么样？".to_string(),
            ],
        );
        assert_eq!(base.len(), asked.len(), "the set is unchanged: {asked:?}");
        assert!(
            asked
                .iter()
                .all(|requirement| requirement.kind != RequirementKind::Tests),
            "a question does not create a tests demand: {asked:?}"
        );
        // An IMPERATIVE amendment still adds scope.
        let instructed = extract(objective, &["再创建 stage-3.txt。".to_string()]);
        assert!(
            instructed.len() > base.len()
                || instructed
                    .iter()
                    .any(|requirement| requirement.text.contains("stage-3.txt")),
            "a real instruction is still scope authority: {instructed:?}"
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

    /// A backtick can quote a fact about the code ("a decoder marks a record
    /// `Valid = false`"), not a deliverable. Demanding an account for the
    /// expression creates a requirement the user never made.
    #[test]
    fn a_backticked_expression_is_a_fact_not_a_deliverable() {
        let requirements = extract(
            "A decoder marks a record `Valid = false` when it could read the record's \
             shape but not its contents.",
            &[],
        );
        assert!(requirements.is_empty(), "{requirements:?}");
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

    /// A repository's own suite name is recognised when it answers a test
    /// demand, but it does not swallow the command requirement when the user
    /// only named the command. `` `make regression` `` without "the tests" is
    /// work to run, not a Tests requirement.
    #[test]
    fn a_project_suite_span_stays_a_command_requirement_when_no_tests_are_demanded() {
        let requirements = extract("先 `make up`，然后跑一下 `make regression`。", &[]);
        assert!(
            requirements.iter().any(|requirement| {
                requirement.text == "make regression"
                    && requirement.kind == RequirementKind::Command
            }),
            "the command the user named must stay a mechanical requirement: {requirements:?}"
        );
        assert!(
            !requirements
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "naming a command is not demanding the test suite: {requirements:?}"
        );
    }

    /// The exact T3/T4 shape: a Go package pattern is not a filesystem path.
    /// Demanding that `./...` exist refuses every correct completion, because
    /// no file can ever satisfy the literal pattern.
    #[test]
    fn a_go_package_wildcard_is_not_an_artifact() {
        let requirements = extract(
            "Do not change existing tests. `go build ./...` and `go test ./...` must pass.",
            &[],
        );
        assert!(
            !requirements
                .iter()
                .any(|requirement| requirement.text == "./..."),
            "a package wildcard is not a path: {requirements:?}"
        );
        assert!(
            requirements.iter().any(|requirement| {
                requirement.text == "go build ./..." && requirement.kind == RequirementKind::Command
            }),
            "the build command remains a mechanical requirement: {requirements:?}"
        );
    }

    #[test]
    fn a_glob_pattern_is_not_an_artifact() {
        let requirements = extract("Update `src/*.rs` and `docs/**/*.md`.", &[]);
        assert!(
            !requirements.iter().any(|requirement| {
                matches!(requirement.kind, RequirementKind::Artifact { .. })
                    && requirement.text.contains('*')
            }),
            "a glob names a set, not one path: {requirements:?}"
        );
    }

    #[test]
    fn a_separator_between_two_tokens_is_not_an_artifact() {
        let requirements = extract("The `count=`/`total=` format is unchanged.", &[]);
        assert!(
            !requirements.iter().any(|requirement| {
                matches!(requirement.kind, RequirementKind::Artifact { .. })
                    && !requirement.text.chars().any(|c| c.is_alphanumeric())
            }),
            "punctuation between two tokens is not a path: {requirements:?}"
        );
    }

    #[test]
    fn the_live_multi_stage_prompt_yields_paths_and_tests_only() {
        let text = "这是一次需要分三个阶段完成的开发任务，请一次做完，中间不要停下来让我继续：\
                    1) 创建 stage1.py，提供 add(a, b)；\
                    2) 创建 stage2.py，提供 mul(a, b)；\
                    3) 创建 stage3.py，提供 slugify(s)（小写并用连字符替换空格）。\
                    最后运行 `python3 -m unittest`，全部通过后再结束。不要修改 test_flow.py。";
        let requirements = extract(text, &[]);
        let found = texts(&requirements);
        for expected in ["stage1.py", "stage2.py", "stage3.py"] {
            assert!(found.contains(&expected), "missing {expected}: {found:?}");
        }
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "{requirements:?}"
        );
        assert!(
            !found.contains(&"test_flow.py"),
            "a prohibited path is not a deliverable: {found:?}"
        );
        assert!(
            !found.iter().any(|text| {
                text.contains("add(") || text.contains("mul(") || text.contains("slugify(")
            }),
            "function signatures are not deliverables: {found:?}"
        );
    }

    #[test]
    fn a_chinese_comma_sequence_is_not_an_enumeration() {
        let requirements = extract(
            "分两阶段完成这个任务：先创建 stage-1.txt，再运行 sh ./verify.sh，\
             完成前不要结束。",
            &[],
        );
        let found = texts(&requirements);
        assert!(found.contains(&"stage-1.txt"), "{found:?}");
        assert!(
            requirements.iter().any(|requirement| {
                requirement.text == "./verify.sh" && requirement.kind == RequirementKind::Command
            }),
            "{requirements:?}"
        );
        assert!(
            !found
                .iter()
                .any(|text| text.contains("不要") || text.contains("结束")),
            "a sequence instruction is not a deliverable: {found:?}"
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

    /// Markdown section numbering is document structure, not a deliverable.
    /// Before this, `## 一、项目目标` was split at `、` into the requirements
    /// `## 一` and `项目目标`, so no honest acceptance list could ever cover the
    /// user's request and `commit_task` was refused for a normal spec.
    #[test]
    fn markdown_section_headings_are_not_deliverables() {
        let text = "# Phase 3C 开发任务\n\n\
                    ## 一、项目目标\n\n在现有项目中实现内容社区功能。\n\n\
                    ## 二、后端实现\n\n使用 Go + PostgreSQL 完成业务接口。\n\n\
                    ## 三、前端实现\n\n实现文章、讨论和评论界面。\n\n\
                    ## 四、验收\n\n运行测试，验证功能完整性。\n\n\
                    请自主规划并执行。\n允许根据任务需要拆分并委托子任务。";
        let requirements = extract(text, &[]);
        let found = texts(&requirements);
        assert!(
            !found.iter().any(|text| text.contains('#')),
            "a heading marker is not a requirement: {found:?}"
        );
        for heading in ["项目目标", "后端实现", "前端实现", "验收"] {
            assert!(
                !found.contains(&heading),
                "a section title is not a deliverable: {found:?}"
            );
        }
        for deliverable in ["文章", "讨论和评论界面"] {
            assert!(
                found.contains(&deliverable),
                "the enumerated deliverable must survive: {found:?}"
            );
        }
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "the explicit test demand must survive: {requirements:?}"
        );
    }

    /// The exact DF-24 prompt: three numbered deliverables, their contents, a
    /// named script, and an explicit "do not stop to ask" clause. The runtime
    /// must owe exactly the three paths and the command — no invented
    /// requirement from the prose around them.
    #[test]
    fn the_df24_recovery_prompt_yields_three_paths_and_one_command() {
        let text = "完成三个阶段的文件交付：\n\
                    1. 创建 stage-1.txt，内容为 stage 1。\n\
                    2. 创建 stage-2.txt，内容为 stage 2。\n\
                    3. 创建 stage-3.txt，内容为 stage 3。\n\
                    三个阶段全部完成后，执行 sh ./verify.sh。\n\
                    只有三个文件全部满足要求，并且验收脚本真实运行成功，才能结束任务。\n\
                    不需要每完成一个阶段就询问用户，也不需要等待用户再次输入「继续」。";
        let requirements = extract(text, &[]);
        let found = texts(&requirements);
        assert_eq!(found.len(), 4, "{requirements:?}");
        for expected in ["stage-1.txt", "stage-2.txt", "stage-3.txt"] {
            assert!(found.contains(&expected), "missing {expected}: {found:?}");
        }
        assert!(
            requirements.iter().any(|requirement| {
                requirement.text == "./verify.sh" && requirement.kind == RequirementKind::Command
            }),
            "{requirements:?}"
        );
    }

    #[test]
    fn an_alternative_slash_is_not_a_path() {
        let requirements = extract(
            "finish all five requirements in app.py (add, mul, slugify, mean, \
             Store.put/get) so that `python3 -m unittest` passes.",
            &[],
        );
        let found = texts(&requirements);
        assert!(found.contains(&"app.py"), "{found:?}");
        assert!(
            !found.contains(&"Store.put/get"),
            "put/get is an alternative, not an artifact: {found:?}"
        );
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "{requirements:?}"
        );
        assert!(
            !found.contains(&"test_app.py"),
            "a prohibited path is not a deliverable: {found:?}"
        );
    }

    #[test]
    fn a_path_in_a_prohibition_is_not_a_requirement() {
        let requirements = extract("不要修改 src/legacy.rs，只改 src/new.rs。", &[]);
        let found = texts(&requirements);
        assert!(!found.contains(&"src/legacy.rs"), "{found:?}");
        assert!(found.contains(&"src/new.rs"), "{found:?}");
    }

    /// "you may A, B, and update `docs/`" permits the whole list. The comma
    /// does not end the permission, and a delivery verb in a later item does
    /// not turn that item into an obligation. A later sentence, and a clause
    /// that itself says must, still name real deliverables.
    #[test]
    fn a_permitted_path_is_not_a_required_artifact() {
        let requirements = extract(
            "Acceptance: `go test ./...` passes.\n\
             Scope: you may add or change `*_test.go` files anywhere, add fixtures under \
             a `testdata/` directory, and update `docs/`. Every OTHER change must be \
             production code in the package where the defect actually lives.\n\
             Changes must stay inside `internal/window/`.\n\
             You may update `notes/`, but you must update `README.md`.\n\
             You may create `docs/a.md`.\n\
             Create `docs/guide.md`.",
            &[],
        );
        let found = texts(&requirements);
        for absent in ["docs/", "testdata/", "notes/", "docs/a.md", "*_test.go"] {
            assert!(!found.contains(&absent), "{absent} is permitted: {found:?}");
        }
        let chinese = extract("可以更新 `docs/`，必须更新 `README.md`。", &[]);
        let chinese = texts(&chinese);
        assert!(!chinese.contains(&"docs/"), "{chinese:?}");
        assert!(chinese.contains(&"README.md"), "{chinese:?}");
        for present in ["internal/window/", "README.md", "docs/guide.md"] {
            assert!(found.contains(&present), "missing {present}: {found:?}");
        }
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "a required test run stays a requirement: {requirements:?}"
        );
    }

    /// A hard-wrapped sentence is still one sentence. The permission does not
    /// end at the line break, so the paths it lists stay permitted rather than
    /// becoming demands the user never made.
    #[test]
    fn a_wrapped_permission_still_permits() {
        let requirements = extract(
            "Scope: you may add or change `*_test.go` files anywhere, add fixtures under\n\
             a `testdata/` directory, and update `docs/`. Every OTHER change must be\n\
             production code in the package where the defect actually lives.",
            &[],
        );
        let found = texts(&requirements);
        for absent in ["testdata/", "docs/", "*_test.go"] {
            assert!(!found.contains(&absent), "{absent} is permitted: {found:?}");
        }
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

    /// The DF-06 shape: a backticked path in a deletion clause. Reading the
    /// backticked span as an artifact without its clause made the runtime
    /// demand that the file the user asked to delete keep existing, while the
    /// bare-path scan produced the opposite requirement. The pair was
    /// unsatisfiable, so completion could only loop (30 rounds, 661k tokens).
    #[test]
    fn a_backticked_path_in_a_deletion_clause_expects_absence() {
        let requirements = extract(
            "First delete `src/AGENTS.md`. Then add `pub fn add(a: i32, b: i32) -> i32` to \
             `src/lib.rs`. No file header is required any more.",
            &[],
        );
        let deleted: Vec<&Requirement> = requirements
            .iter()
            .filter(|requirement| requirement.text == "src/AGENTS.md")
            .collect();
        assert_eq!(
            deleted.len(),
            1,
            "one fact per path, never a presence and an absence claim: {requirements:?}"
        );
        assert_eq!(
            deleted[0].kind,
            RequirementKind::Artifact { removed: true },
            "the clause says delete: {requirements:?}"
        );
    }

    /// A deletion and a creation in one clause keep their own verbs. The
    /// deletion verb must not travel to the path named after it.
    #[test]
    fn a_removal_verb_does_not_apply_to_a_later_path_in_the_same_clause() {
        let requirements = extract("Delete `old.txt` and create `new.txt`.", &[]);
        let old = requirements
            .iter()
            .find(|requirement| requirement.text == "old.txt")
            .expect("old.txt");
        let new = requirements
            .iter()
            .find(|requirement| requirement.text == "new.txt")
            .expect("new.txt");
        assert_eq!(old.kind, RequirementKind::Artifact { removed: true });
        assert_eq!(new.kind, RequirementKind::Artifact { removed: false });
    }

    /// Two statements about one path must not become two contradictory
    /// requirements. The later one is the user's final word.
    #[test]
    fn opposite_statements_about_one_path_keep_only_the_last() {
        for (text, removed) in [
            ("Create `a.txt`. Delete `a.txt`.", true),
            ("Delete `a.txt`. Create `a.txt`.", false),
        ] {
            let requirements = extract(text, &[]);
            let found: Vec<&Requirement> = requirements
                .iter()
                .filter(|requirement| requirement.text == "a.txt")
                .collect();
            assert_eq!(found.len(), 1, "one fact per path: {requirements:?}");
            assert_eq!(
                found[0].kind,
                RequirementKind::Artifact { removed },
                "{text}: {requirements:?}"
            );
        }
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
    fn a_broad_acceptance_sentence_cannot_cover_specific_deliverables() {
        let requirements = extract(
            "1. 在 app.py 增加 archive_article。\n2. 在 tests/test_app.py 补两个用例。\n3. 实现全部接口并通过测试。\n4. 运行 sh ./verify.sh。",
            &[],
        );
        let missing = uncovered(&requirements, &["完成全部功能并且测试通过".to_string()]);
        assert!(
            !missing.is_empty(),
            "a blanket claim must not satisfy named deliverables: {requirements:?}"
        );
        assert!(missing.contains(&"app.py".to_string()), "{missing:?}");
        assert!(
            missing.contains(&"tests/test_app.py".to_string()),
            "{missing:?}"
        );
        assert!(
            missing.contains(&"./verify.sh".to_string()),
            "a broad claim does not cover the named command: {missing:?}"
        );
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
        let parsed = parse_accounting(
            &serde_json::json!({
                "requirements": [
                    {"requirement": "stage-1.txt", "status": "done", "evidence": "created it"},
                    {"requirement": "stage-2.txt", "status": "not_done", "evidence": "not reached"},
                    {"requirement": "quality", "status": "unverifiable", "evidence": "semantic"}
                ]
            }),
            20,
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].status, AccountStatus::Done);
        assert_eq!(parsed[1].status, AccountStatus::NotDone);
        assert_eq!(parsed[2].status, AccountStatus::Unverifiable);

        assert!(
            parse_accounting(&serde_json::json!({}), 20)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_accounting(
                &serde_json::json!({
                    "requirements": [{"requirement": "x", "status": "skipped", "evidence": "no"}]
                }),
                20
            )
            .is_err()
        );
        assert!(
            parse_accounting(
                &serde_json::json!({
                    "requirements": [{"requirement": "x", "status": "done", "evidence": "  "}]
                }),
                20
            )
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

    /// The user names a full path once and its bare filename later; the model
    /// accounts for the filename and puts the full path in the evidence. The
    /// artifact is mechanically present, and the account demonstrably saw it:
    /// exact-quote matching alone would refuse a completion the runtime can
    /// verify.
    #[test]
    fn an_account_whose_evidence_names_the_requirement_covers_it() {
        let requirements = vec![req(
            "internal/report/aggregate.go",
            RequirementKind::Artifact { removed: false },
        )];
        let present = |_: &str| ArtifactState::Present;
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account(
                "aggregate.go contains only Aggregate",
                AccountStatus::Done,
                "internal/report/aggregate.go read after the edit; only Aggregate remains",
            )]),
            runs: &[],
            mutation_ops: 1,
            artifact: &present,
        });
        assert!(
            matches!(decision, ScopeDecision::Accept { .. }),
            "{decision:?}"
        );
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

    /// The user asked to run the tests; the repository's suite is
    /// `make regression`, not `make test`. The runtime recorded that command
    /// and it passed after the last change, so the requirement is met — the
    /// refusal that greeted this exact record (session `1c3bf9aa`, 2026-10-30)
    /// said no test command had run.
    #[test]
    fn a_project_suite_named_regression_satisfies_a_test_requirement() {
        let requirements = vec![req("tests", RequirementKind::Tests)];
        let present = |_: &str| ArtifactState::Present;
        let runs = vec![
            run(
                "bash -lc make regression > tmp/run/regression.log 2>&1; echo \"regression 退出码: $?\"",
                true,
                5,
                0,
            ),
            run("make regression", true, 25, 0),
        ];
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[account(
                "tests",
                AccountStatus::Done,
                "make regression（前台，exit 0）：13 组用例，191 条断言通过",
            )]),
            runs: &runs,
            mutation_ops: 0,
            artifact: &present,
        });
        assert!(
            matches!(decision, ScopeDecision::Accept { .. }),
            "{decision:?}"
        );

        // Starting the services is not running the suite, and the refusal
        // says which names a test run needs.
        let only_started = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: None,
            runs: &[run("make up", true, 3, 0), run("make status", true, 4, 0)],
            mutation_ops: 0,
            artifact: &present,
        });
        match only_started {
            ScopeDecision::Refuse(message) => {
                assert!(message.contains("no test command ran"), "{message}");
                assert!(message.contains("regression"), "{message}");
            }
            other => panic!("expected refusal, got {other:?}"),
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

    /// The exact RC3 shape: a file-name format illustrated with `e.g.
    /// `p01.txt``. An illustration is not a deliverable; extracting it makes
    /// a correct completion impossible because no such file is owed.
    #[test]
    fn an_illustrated_example_is_not_a_deliverable() {
        assert!(
            example_context("the exact file name, e.g. `p01.txt`)", "p01.txt"),
            "helper must see the immediate example"
        );
        let requirements = extract(
            "`data/chain.txt` starts with a line `NEXT=<file>`. Follow that link \
             (files live in `data/`; `<file>` is the exact file name, e.g. `p01.txt`).\
             \nWrite the answer into `src/answer.txt`.",
            &[],
        );
        let found = texts(&requirements);
        assert!(
            !found.contains(&"p01.txt"),
            "an illustrated example is not a deliverable: {found:?}"
        );
        assert!(found.contains(&"data/chain.txt"), "{found:?}");
        assert!(
            found.contains(&"src/answer.txt"),
            "a real deliverable later in the request must remain: {found:?}"
        );
    }

    /// RC3 used `N=` in a requirement sentence. Single letters in plain
    /// prose or longer identifiers must not stand in for the requested name.
    #[test]
    fn a_one_character_requirement_matches_an_explicit_identifier() {
        let requirements = extract(
            "Keep following `NEXT` until it says `NEXT=END`, summing every `N` value you pass.",
            &[],
        );
        let n = requirements
            .iter()
            .find(|requirement| requirement.text == "N")
            .expect("the value name is a requirement");
        let accept = decide(&ScopeFacts {
            requirements: std::slice::from_ref(n),
            accounting: Some(&[account(
                "`N`",
                AccountStatus::Done,
                "summed every N value to 296",
            )]),
            runs: &[],
            mutation_ops: 0,
            artifact: &|_| ArtifactState::Unknown,
        });
        assert!(
            matches!(accept, ScopeDecision::Accept { .. }),
            "an exact account must satisfy a one-character requirement: {accept:?}"
        );

        let a = req("A", RequirementKind::Named);
        let i = req("I", RequirementKind::Named);
        for (requirement, label, expected) in [
            (n, "Read the N= integer line in each file", true),
            (n, "Read the N = integer line in each file", true),
            (n, "Sum every `N` value", true),
            (n, "Sum every N value", false),
            (n, "Read the COUNT_N= field", false),
            (&a, "Read a file", false),
            (&i, "I read the file", false),
        ] {
            let decision = decide(&ScopeFacts {
                requirements: std::slice::from_ref(requirement),
                accounting: Some(&[account(
                    label,
                    AccountStatus::Done,
                    "summed every N value to 296",
                )]),
                runs: &[],
                mutation_ops: 0,
                artifact: &|_| ArtifactState::Unknown,
            });
            assert_eq!(
                matches!(decision, ScopeDecision::Accept { .. }),
                expected,
                "{label}: {decision:?}"
            );
        }
    }

    /// A prohibition the user stated is a constraint, not a deliverable. It
    /// must never be extracted as work, never need an `acceptance` check, and
    /// never be accounted as `done` — "禁止实现支付功能" cannot be "delivered".
    #[test]
    fn a_prohibition_is_a_constraint_and_never_a_deliverable() {
        let requirements = extract(
            "## 三、禁止事项\n\n- 禁止实现支付功能。\n- 不要新增第三方登录。\n\
             - 不得修改线上配置。\n",
            &[],
        );
        assert!(
            !requirements.is_empty()
                && requirements
                    .iter()
                    .all(|requirement| requirement.kind == RequirementKind::Forbidden),
            "every item under a prohibition heading is forbidden, not a deliverable: {requirements:?}"
        );
        assert!(
            uncovered(&requirements, &[]).is_empty(),
            "a prohibition never needs an acceptance check"
        );
        let decision = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: None,
            runs: &[],
            mutation_ops: 0,
            artifact: &|_| ArtifactState::Unknown,
        });
        assert!(
            matches!(decision, ScopeDecision::Accept { .. }),
            "a prohibition must not block completion: {decision:?}"
        );
    }

    /// A constraint clause can state its prohibitions without a heading or a
    /// bullet list. They still belong to the contract as constraints: the
    /// model must see them, but neither `commit_task` coverage nor the
    /// completion account may turn them into delivered work. Their presence
    /// must not manufacture an uncoverable deliverable either.
    #[test]
    fn a_standalone_constraint_clause_keeps_its_prohibitions_structured() {
        let requirements = extract(
            "在 app.py 增加 word_count(text)；在 tests/test_app.py 增加一个用例；             运行 sh ./verify.sh。约束：禁止新增第三方依赖，禁止修改 app.py 和              tests/test_app.py 以外的文件。",
            &[],
        );
        let forbidden: Vec<&str> = requirements
            .iter()
            .filter(|requirement| requirement.kind == RequirementKind::Forbidden)
            .map(|requirement| requirement.text.as_str())
            .collect();
        assert!(
            forbidden.iter().any(|text| text.contains("第三方依赖")),
            "the dependency prohibition stays structured: {requirements:?}"
        );
        assert!(
            forbidden.iter().any(|text| text.contains("以外的文件")),
            "the scope prohibition stays structured: {requirements:?}"
        );
        let missing = uncovered(&requirements, &[]);
        assert!(
            !missing
                .iter()
                .any(|text| forbidden.contains(&text.as_str())),
            "a prohibition never needs an acceptance check: {missing:?}"
        );
    }

    /// The user described a pipeline with an ideographic-comma list inside a
    /// comma clause. Those are not three enumerated deliverables; splitting
    /// them produced fragments no acceptance list could ever cover.
    #[test]
    fn a_descriptive_ideographic_list_is_not_an_enumeration() {
        let requirements = extract(
            "在现有 Phase 3B 基础上，把文章、讨论、评论三条社区业务链路做成真实可用的闭环，\
             允许真实用户发布、回复与浏览。",
            &[],
        );
        assert!(
            requirements.is_empty(),
            "a describing clause enumerates nothing: {requirements:?}"
        );
    }

    /// The instruction clause is still an enumeration: only the structural
    /// gate changed, not the extraction of real deliverable lists.
    #[test]
    fn an_instruction_clause_still_enumerates_its_deliverables() {
        let requirements = extract("实现文章发布、讨论区浏览、评论回复。", &[]);
        let found = texts(&requirements);
        for expected in ["文章发布", "讨论区浏览", "评论回复"] {
            assert!(found.contains(&expected), "missing {expected}: {found:?}");
        }
    }

    /// A numbered deliverable used to be extracted twice — once with its `1.`
    /// marker (which no acceptance check can cover) and once without.
    #[test]
    fn a_numbered_item_is_extracted_once_without_its_marker() {
        let requirements = extract("1. 实现文章发布与编辑，包括草稿、发布、下架三种状态。", &[]);
        let found = texts(&requirements);
        assert_eq!(
            found,
            vec!["文章发布与编辑"],
            "one numbered deliverable, no marker, no fragment list: {requirements:?}"
        );
        assert!(
            uncovered(&requirements, &["文章发布与编辑可访问".to_string()],).is_empty(),
            "the deliverable is coverable: {requirements:?}"
        );
    }

    /// The reconstructed Phase 3C sample: a real complex Chinese Markdown
    /// objective. Every deliverable survives, prose fragments and prohibitions
    /// do not become deliverables, and the whole contract is coverable.
    #[test]
    fn a_complex_chinese_markdown_objective_extracts_only_its_deliverables() {
        let objective =
            include_str!("../../../../testdata/regression/phase3c_requirement.zh-CN.md");
        let requirements = extract(objective, &[]);
        let deliverables: Vec<&str> = requirements
            .iter()
            .filter(|requirement| requirement.kind.is_deliverable())
            .map(|requirement| requirement.text.as_str())
            .collect();
        for expected in [
            "文章发布与编辑",
            "讨论区主题创建与分页浏览",
            "评论回复",
            "内容权限校验",
            "真实浏览器 E2E",
            "接口层单元测试",
        ] {
            assert!(
                deliverables.contains(&expected),
                "missing deliverable {expected}: {deliverables:?}"
            );
        }
        for fragment in ["讨论", "回复", "浏览", "发布", "在现有 Phase 3B 基础上"] {
            assert!(
                !deliverables.contains(&fragment),
                "a prose fragment became a deliverable: {fragment} in {deliverables:?}"
            );
        }
        let forbidden: Vec<&str> = requirements
            .iter()
            .filter(|requirement| requirement.kind == RequirementKind::Forbidden)
            .map(|requirement| requirement.text.as_str())
            .collect();
        for expected in ["禁止实现支付功能", "不要新增第三方登录"] {
            assert!(
                forbidden.contains(&expected),
                "missing prohibition {expected}: {forbidden:?}"
            );
        }
        assert!(
            requirements
                .iter()
                .any(|requirement| requirement.kind == RequirementKind::Tests),
            "the explicit test demand survives: {requirements:?}"
        );
        assert!(
            deliverables.len() <= 12,
            "a complex but legible objective must not explode into fragments: {deliverables:?}"
        );

        // One accounting vocabulary for the report: named deliverables and a
        // test demand are both deliverable-class requirements; prohibitions
        // are the negative constraints and are never part of that count.
        let named = requirements
            .iter()
            .filter(|requirement| requirement.kind == RequirementKind::Named)
            .count();
        let tests = requirements
            .iter()
            .filter(|requirement| requirement.kind == RequirementKind::Tests)
            .count();
        let forbidden_count = forbidden.len();
        assert_eq!((named, tests, forbidden_count), (6, 1, 3));
        assert_eq!(deliverables.len(), named + tests);
        assert_eq!(requirements.len(), named + tests + forbidden_count);
    }

    /// The accounting cap follows the requirement set. A fixed 20 made a goal
    /// with more requirements structurally impossible to complete.
    #[test]
    fn a_long_goal_can_account_for_every_requirement_it_has() {
        let items: Vec<serde_json::Value> = (1..=30)
            .map(|index| {
                serde_json::json!({
                    "requirement": format!("item-{index}"),
                    "status": "done",
                    "evidence": "shipped",
                })
            })
            .collect();
        let arguments = serde_json::json!({ "requirements": items });
        assert!(
            parse_accounting(&arguments, 30).is_ok(),
            "30 requirements must be accountable"
        );
        assert!(
            parse_accounting(&arguments, 10).is_err(),
            "a smaller goal keeps a bound"
        );
        // The floor is a payload bound, not a scope statement.
        let twenty = serde_json::json!({
            "requirements": (1..=20)
                .map(|index| serde_json::json!({
                    "requirement": format!("item-{index}"),
                    "status": "done",
                    "evidence": "shipped",
                }))
                .collect::<Vec<_>>(),
        });
        assert!(parse_accounting(&twenty, 1).is_ok());
    }

    // ---- Chinese deliverable boundaries -----------------------------------
    //
    // One owner decided the boundary between "the user listed several
    // deliverables" and "the user named one thing": the extractor, plus the
    // structural part reading in `named_parts`. Every case below asserts both
    // the parse and the verdict, because a parser that only avoids panicking
    // can still refuse an honest contract or pass a partial one.

    fn uncovered_for(objective: &str, acceptance: &[&str]) -> Vec<String> {
        let requirements = extract(objective, &[]);
        let checks: Vec<String> = acceptance
            .iter()
            .map(|check| (*check).to_string())
            .collect();
        uncovered(&requirements, &checks)
    }

    /// The whole list, with one concrete check per part. This is what the tool
    /// schema asks for and what the nail-down of DF-01 expects to pass.
    #[test]
    fn an_acceptance_that_names_each_part_of_a_joined_list_is_accepted() {
        let left = uncovered_for(
            "## 三、前端实现\n\n实现文章、讨论和评论界面。\n",
            &["文章列表可访问", "讨论区可访问", "评论界面可访问"],
        );
        assert!(left.is_empty(), "each part is named: {left:?}");
    }

    /// The same list with the middle deliverable dropped must still be refused.
    /// Accepting the per-part reading above is only honest if it keeps this.
    #[test]
    fn dropping_a_part_of_a_joined_list_is_refused() {
        let left = uncovered_for(
            "## 三、前端实现\n\n实现文章、讨论和评论界面。\n",
            &["文章列表可访问", "评论界面可访问"],
        );
        assert_eq!(
            left,
            vec!["讨论和评论界面".to_string()],
            "the undelivered part is what the refusal names: {left:?}"
        );
    }

    /// DF-02 shape: two bullet deliverables, an acceptance list that covers
    /// only one. The refusal names the deliverable the user stated.
    #[test]
    fn a_two_item_bullet_list_refuses_a_partial_acceptance() {
        let objective = "## 二、必须交付\n\n- 订单管理接口\n- 支付接口\n";
        let partial = uncovered_for(objective, &["订单管理接口可访问"]);
        assert_eq!(
            partial,
            vec!["支付接口".to_string()],
            "the uncovered deliverable is named exactly: {partial:?}"
        );
        let complete = uncovered_for(objective, &["订单管理接口可访问", "支付接口可访问"]);
        assert!(complete.is_empty(), "{complete:?}");
    }

    /// `研发与交付平台` is one business name. The literal phrase must stay
    /// acceptable, and a check that only names one half must not certify the
    /// whole.
    #[test]
    fn a_conjunction_inside_a_business_name_is_not_a_deliverable_list() {
        let objective = "- 研发与交付平台\n";
        assert!(
            uncovered_for(objective, &["研发与交付平台已上线"]).is_empty(),
            "the user's own phrase is always an acceptable check"
        );
        assert_eq!(
            uncovered_for(objective, &["研发流水线通过"]),
            vec!["研发与交付平台".to_string()],
            "half of one name does not certify it"
        );
    }

    /// `CI/CD` is a technical token, not a two-item list. Splitting it on the
    /// slash made `ci` and `cd` into requirements that unrelated checks
    /// satisfied — a false acceptance path, not a false rejection one.
    #[test]
    fn a_technical_slash_is_not_a_deliverable_list() {
        let objective = "- CI/CD 流水线\n";
        assert_eq!(
            named_parts("ci/cd 流水线"),
            vec!["ci/cd 流水线".to_string()]
        );
        assert!(
            uncovered_for(objective, &["CI/CD 流水线通过"]).is_empty(),
            "the token itself is the check"
        );
        assert_eq!(
            uncovered_for(objective, &["CI 流水线通过"]),
            vec!["CI/CD 流水线".to_string()],
            "`ci` alone does not name the pipeline"
        );
    }

    /// A conjunction that opens a word is part of the word. `和平协议` and
    /// `及时通知` are one deliverable each, and a check naming the remainder
    /// does not cover them.
    #[test]
    fn a_conjunction_that_opens_a_word_is_not_a_list() {
        let objective = "- 和平协议文档\n- 及时通知能力\n";
        assert_eq!(
            named_parts("和平协议文档"),
            vec!["和平协议文档".to_string()],
            "a leading 和 is not a coordinator"
        );
        assert_eq!(
            named_parts("及时通知能力"),
            vec!["及时通知能力".to_string()],
            "a leading 及 is not a coordinator"
        );
        assert_eq!(
            uncovered_for(objective, &["协议文档已归档", "通知能力已实现"]),
            vec!["和平协议文档".to_string(), "及时通知能力".to_string()],
            "neither word is covered by its remainder"
        );
        assert!(
            uncovered_for(objective, &["和平协议文档已归档", "及时通知能力已实现"]).is_empty(),
            "the words themselves are accepted"
        );
    }

    /// A genuine list of two inside one bullet: each part is owed.
    #[test]
    fn a_conjoined_list_of_two_is_owed_part_by_part() {
        let objective = "- 订单和支付模块\n";
        assert!(
            uncovered_for(objective, &["订单和支付模块可用"]).is_empty(),
            "the literal phrase covers both parts"
        );
        assert!(uncovered_for(objective, &["订单模块可访问", "支付模块可访问"]).is_empty());
        assert_eq!(
            uncovered_for(objective, &["订单模块可访问"]),
            vec!["订单和支付模块".to_string()],
            "one module is not the pair"
        );
    }

    /// Mixed Chinese and English deliverables, with the prose around them
    /// carrying no requirement at all.
    #[test]
    fn a_mixed_language_list_yields_each_deliverable() {
        let objective = "1. 实现 Go 服务的 user API，并提供 swagger 文档。\n\
                         2. 实现 admin 后台，使用 React。\n";
        let requirements = extract(objective, &[]);
        assert_eq!(
            texts(&requirements),
            vec!["Go 服务的 user API", "admin 后台"],
            "two deliverables, no fragments: {requirements:?}"
        );
        assert!(
            uncovered(
                &requirements,
                &[
                    "Go 服务的 user API 提供 swagger 文档".to_string(),
                    "admin 后台使用 React 实现".to_string(),
                ],
            )
            .is_empty(),
            "checks quoting each deliverable cover them"
        );
        assert_eq!(
            uncovered(&requirements, &["admin 后台使用 React 实现".to_string()]),
            vec!["Go 服务的 user API".to_string()]
        );
    }

    /// A nested markdown list must not turn list markers, headings or the
    /// parent item into extra deliverables.
    #[test]
    fn a_nested_markdown_list_does_not_invent_deliverables() {
        let objective = "## 一、交付\n\n1. 用户管理\n   - 创建用户\n   - 删除用户\n\
                         2. 文章管理\n\n## 二、验收\n\n运行 `go test ./...`。\n";
        let requirements = extract(objective, &[]);
        let found = texts(&requirements);
        assert!(
            !found
                .iter()
                .any(|text| text.contains('#') || text.contains("交付") || text.contains("验收")),
            "document structure is not a deliverable: {found:?}"
        );
        for expected in ["用户管理", "文章管理"] {
            assert!(found.contains(&expected), "missing {expected}: {found:?}");
        }
        assert!(
            uncovered(
                &requirements,
                &[
                    "用户管理可访问（创建与删除）".to_string(),
                    "文章管理可访问".to_string(),
                    "go test ./... 通过".to_string(),
                ],
            )
            .is_empty(),
            "{requirements:?}"
        );
    }

    /// Background prose — a historical note about what an earlier phase
    /// already shipped — is not a deliverable, and the acceptance list is
    /// judged only against what the user asked for now.
    #[test]
    fn background_prose_is_not_judged_as_a_deliverable() {
        let objective = "## 背景\n\nPhase 3B 已完成用户体系与内容模型，本阶段只扩展社区交互。\n\n\
                         ## 交付\n\n- 实现评论服务\n";
        let requirements = extract(objective, &[]);
        let found = texts(&requirements);
        assert_eq!(
            found,
            vec!["评论服务"],
            "only the stated deliverable is owed: {requirements:?}"
        );
        assert!(
            uncovered_for(objective, &["评论服务可访问"]).is_empty(),
            "the background does not add a check the model must satisfy"
        );
    }

    /// The completion gate must read a joined phrase the same way the commit
    /// gate does. Before this, per-part checks were accepted at commit and a
    /// per-part account was then refused as "not accounted for".
    #[test]
    fn per_part_accounts_satisfy_a_joined_deliverable_at_completion() {
        let requirements = extract("实现文章、讨论和评论界面。", &[]);
        let accept = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[
                account("文章", AccountStatus::Done, "文章列表与详情页已上线"),
                account("讨论", AccountStatus::Done, "讨论区已上线"),
                account("评论界面", AccountStatus::Done, "评论界面已上线"),
            ]),
            runs: &[],
            mutation_ops: 0,
            artifact: &|_| ArtifactState::Unknown,
        });
        assert!(
            matches!(accept, ScopeDecision::Accept { .. }),
            "the account mirrors the accepted checks: {accept:?}"
        );

        let refuse = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[
                account("文章", AccountStatus::Done, "文章列表与详情页已上线"),
                account("讨论", AccountStatus::Done, "讨论区已上线"),
                account("评论界面", AccountStatus::NotDone, "评论界面未做"),
            ]),
            runs: &[],
            mutation_ops: 0,
            artifact: &|_| ArtifactState::Unknown,
        });
        match refuse {
            ScopeDecision::Refuse(message) => assert!(
                message.contains("讨论和评论界面") && message.contains("not_done"),
                "{message}"
            ),
            other => panic!("a part marked not_done must refuse: {other:?}"),
        }

        let missing = decide(&ScopeFacts {
            requirements: &requirements,
            accounting: Some(&[
                account("文章", AccountStatus::Done, "文章列表与详情页已上线"),
                account("讨论", AccountStatus::Done, "讨论区已上线"),
            ]),
            runs: &[],
            mutation_ops: 0,
            artifact: &|_| ArtifactState::Unknown,
        });
        assert!(
            matches!(missing, ScopeDecision::Refuse(_)),
            "an unaccounted part still refuses: {missing:?}"
        );
    }

    /// The reconstructed Phase 3C objective, judged against a real acceptance
    /// list and against one that silently drops a deliverable.
    #[test]
    fn the_phase3c_acceptance_is_judged_against_its_own_deliverables() {
        let objective =
            include_str!("../../../../testdata/regression/phase3c_requirement.zh-CN.md");
        let complete = [
            "文章发布与编辑（草稿/发布/下架）端到端可用",
            "讨论区主题创建与分页浏览可用",
            "评论回复支持二级回复与删除",
            "内容权限校验：作者可编辑、管理员可下架、普通用户只读",
            "真实浏览器 E2E 覆盖发布、回复、权限拒绝",
            "接口层单元测试通过且覆盖率不低于 70%",
        ];
        let left = uncovered_for(objective, &complete);
        assert!(
            left.is_empty(),
            "a complete acceptance is accepted: {left:?}"
        );

        let dropped = &complete[..5];
        let left = uncovered_for(objective, dropped);
        assert!(
            !left.is_empty(),
            "dropping the test deliverable must be refused: {left:?}"
        );
    }

    /// A counted summary after a list describes the list. It is not part of the
    /// last deliverable's name, and a check that names the deliverable covers
    /// it — while a check that names only the verb still does not.
    #[test]
    fn a_counted_summary_is_not_part_of_the_last_deliverable() {
        let requirements = extract(
            "完成文章发布、评论回复、内容权限校验三个功能，完成后通过测试",
            &[],
        );
        let found = texts(&requirements);
        assert!(found.contains(&"内容权限校验三个功能"), "{found:?}");
        let missing = uncovered(
            &requirements,
            &[
                "文章发布可访问".to_string(),
                "评论回复可提交且可见".to_string(),
                "内容权限校验覆盖作者与管理员".to_string(),
                "sh run-tests.sh 通过".to_string(),
            ],
        );
        assert!(
            missing.is_empty(),
            "the named deliverable is covered: {missing:?}"
        );

        // The stem may never shrink to a verb.
        assert_eq!(strip_counted_summary("支持三种登录方式"), None);
        assert_eq!(
            strip_counted_summary("内容权限校验三个功能"),
            Some("内容权限校验")
        );
        let weak = uncovered(
            &requirements,
            &[
                "文章发布可访问".to_string(),
                "评论回复可提交且可见".to_string(),
                "支持内容".to_string(),
                "sh run-tests.sh 通过".to_string(),
            ],
        );
        assert_eq!(weak, vec!["内容权限校验三个功能".to_string()]);
    }

    /// A labelled bullet names the deliverable AFTER its label.
    /// `阶段 2：在 app.py 增加 articles_by_author` used to extract `阶段 2`, a
    /// section title no acceptance list can honestly contain — and the live
    /// DeepSeek run refused its first `commit_task` over exactly that label.
    #[test]
    fn a_labelled_bullet_states_its_deliverable_not_its_label() {
        let requirements = extract(
            "4. 阶段 2：在 app.py 增加 articles_by_author(author)，并在 tests/test_app.py 补一个用例。",
            &[],
        );
        let found = texts(&requirements);
        assert!(
            !found.contains(&"阶段 2"),
            "a stage label is not a deliverable: {found:?}"
        );
        assert!(
            found.contains(&"tests/test_app.py"),
            "the real artifact is still extracted: {found:?}"
        );
    }

    /// The part reading is strictly additional. A check that names the joined
    /// phrase always covers it, whatever the split would produce.
    #[test]
    fn the_literal_phrase_is_never_less_acceptable_than_its_parts() {
        for phrase in [
            "讨论和评论界面",
            "研发与交付平台",
            "CI/CD 流水线",
            "和平协议文档",
            "及时通知能力",
            "订单和支付模块",
        ] {
            let requirement = req(phrase, RequirementKind::Named);
            let checks = vec![format!("完成{phrase}")];
            assert!(
                uncovered(std::slice::from_ref(&requirement), &checks).is_empty(),
                "the user's own phrase must always be an acceptable check: {phrase}"
            );
        }
    }
}
