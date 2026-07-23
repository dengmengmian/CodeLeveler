use std::path::PathBuf;

use leveler_context::{ProjectInstruction, render_instructions};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;

/// The default system prompt. Lives in `prompts/base.md` rather than a string
/// literal so it can be edited and diffed as prose — and so a model profile can
/// ship its own (see `PromptBuilder::base_instructions`): one prompt cannot fit
/// every model, since a weak model needs the long form with worked examples and
/// a strong one is degraded by it.
const BASE_PROMPT: &str = include_str!("../prompts/base.md");

#[derive(Debug, Clone)]
pub(crate) struct PromptBuilder {
    require_explicit_plan: bool,
    turn_context: Option<TurnContext>,
    base_instructions: Option<String>,
    commit_co_author: bool,
    /// Short memory INDEX (titles only). Empty = omit segment.
    memory_index: String,
}

impl Default for PromptBuilder {
    fn default() -> Self {
        Self {
            require_explicit_plan: false,
            turn_context: None,
            base_instructions: None,
            commit_co_author: true,
            memory_index: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnContext {
    pub(crate) model: ModelRef,
    pub(crate) mode: PermissionProfile,
    pub(crate) network_allowed: bool,
    pub(crate) deny_network: bool,
    pub(crate) cwd: PathBuf,
    pub(crate) project_rules: Vec<ProjectInstruction>,
    /// The language the user is writing in, when we can name it. `None` falls
    /// back to the generic "mirror the user" rule.
    pub(crate) user_language: Option<&'static str>,
}

/// Name the language of a user request, so the prompt can state it outright.
///
/// "Use the same natural language as the latest user message" asks the model to
/// infer the language and then police every sentence against it. A weak model
/// does neither: measured across three real sessions, deepseek-v4-pro broke that
/// rule in 49% of its user-visible messages — interim notes ("Now let me ...")
/// streamed to a user who was writing Chinese. Resolving the language here and
/// naming it turns inference into instruction, which the same model follows.
///
/// Only scripts we can identify from the characters themselves are named; every
/// other language keeps the generic rule rather than being guessed at. Code,
/// paths and quoted identifiers are stripped first — an English request that
/// quotes a Chinese field name is still an English request.
pub(crate) fn user_language(text: &str) -> Option<&'static str> {
    let prose = strip_code(text);
    let mut han = 0usize;
    let mut latin = 0usize;
    for c in prose.chars() {
        if matches!(c, '\u{4e00}'..='\u{9fff}') {
            han += 1;
        } else if c.is_ascii_alphabetic() {
            latin += 1;
        }
    }
    // Chinese prose stays Chinese even when it is mostly identifiers and English
    // technical terms, so weigh a Han character as the word it is.
    const HAN_WEIGHT: usize = 3;
    (han > 0 && han * HAN_WEIGHT >= latin).then_some("Chinese (中文)")
}

/// Drop fenced blocks, inline code and paths: they carry the identifiers of the
/// codebase, not the language the user is speaking.
fn strip_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let mut in_tick = false;
        for c in line.chars() {
            match c {
                '`' => in_tick = !in_tick,
                _ if in_tick => {}
                _ => out.push(c),
            }
        }
        out.push('\n');
    }
    out
}

impl PromptBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn require_explicit_plan(mut self, enabled: bool) -> Self {
        self.require_explicit_plan = enabled;
        self
    }

    pub(crate) fn turn_context(mut self, context: TurnContext) -> Self {
        self.turn_context = Some(context);
        self
    }

    pub(crate) fn commit_co_author(mut self, enabled: bool) -> Self {
        self.commit_co_author = enabled;
        self
    }

    /// Use this model's own system prompt instead of the default. It REPLACES
    /// the base — a model profile's instructions are a whole prompt, not an
    /// addendum — while the turn context, project rules, and the opt-in sections
    /// below still apply on top. None keeps the default.
    pub(crate) fn base_instructions(mut self, instructions: Option<String>) -> Self {
        self.base_instructions = instructions.filter(|s| !s.trim().is_empty());
        self
    }

    /// Inject a short memory INDEX (titles/ids only — never entry bodies).
    pub(crate) fn memory_index(mut self, index: impl Into<String>) -> Self {
        self.memory_index = index.into();
        self
    }

    pub(crate) fn build(&self) -> String {
        let mut prompt = match &self.base_instructions {
            Some(custom) => custom.clone(),
            None => String::from(BASE_PROMPT),
        };
        // Memory INDEX is part of the cache-stable prefix when present: titles
        // only, fixed template, no bodies (K37).
        if !self.memory_index.trim().is_empty() {
            prompt.push_str(
                "\n\n## Project memory index\n\
                 Durable user-approved notes (titles only). Use the `memory` tool \
                 to read bodies. Do not invent memories not listed here.\n",
            );
            prompt.push_str(self.memory_index.trim());
            prompt.push('\n');
        }
        if let Some(context) = &self.turn_context {
            prompt.push_str("\n\n");
            prompt.push_str(&context.render());
            if self.commit_co_author {
                prompt.push_str(&format!(
                    "\n\nWhen you create a git commit, append this exact trailer after a blank \
                     line (unless it is already present):\nCo-Authored-By: CodeLeveler ({}) \
                     <noreply@codeleveler.com>",
                    context.model
                ));
            }
        }
        // Narration contract (K28): map progress to the active plan step when one
        // exists; always cite concrete evidence. Lives outside replaceable model
        // profiles so every model produces interpretable progress.
        prompt.push_str(
            "\n\nProgress narration: when a plan is active, lead with \
             `current step k/n · <step text> — just did … (evidence) → next …`. \
             Every interim update must name concrete evidence you observed, its \
             implication, and the next action. Never emit a bare claim such as \
             \"found the root cause\" without naming what was found and why it \
             matters. Keep updates concise and in the user's language.",
        );
        if self.require_explicit_plan {
            prompt.push_str(
                "\n\nBefore calling any tool, decide if this is multi-step work \
                 (several independently checkable pieces, multi-file changes, or \
                 migrate/architecture work). If yes: after optional read-only \
                 explore, your first substantive action must be update_plan with \
                 one in_progress step and the rest pending (statuses: \
                 pending/in_progress/completed) — not a prose checklist alone — \
                 then follow and revise that plan via update_plan. If a single \
                 action covers the request, skip update_plan and just do the task.",
            );
            // Zero-cost guidance, unlike the removed MissingEvidence nudge: it
            // costs no extra round, and a model that verifies inside the turn
            // can still fix what it finds. The engine's gating checks remain
            // the actual verdict either way.
            prompt.push_str(
                "\n\nFor tasks where you edit files, do NOT declare the task complete \
                 until you have run the build or tests with run_command and seen \
                 them pass. Cite that result as your evidence. For chat, explanation, \
                 or read-only questions, answer directly without verification tools.",
            );
        }
        prompt
    }
}

impl TurnContext {
    fn render(&self) -> String {
        let network = if self.mode == PermissionProfile::FullAccess
            || (self.network_allowed && !self.deny_network)
        {
            "allowed"
        } else {
            "denied"
        };
        let language = match self.user_language {
            Some(named) => format!(
                "- language: the user writes {named}. Write EVERY user-visible sentence in \
                 {named} — interim progress notes, plans, status narration, reasoning/thinking \
                 text streamed to the UI, and the final summary. Do not slip into English \
                 process templates such as \"Now...\", \"First...\", \"Good...\", or \"Let \
                 me...\"; if a draft sentence comes out in the wrong language, rewrite it \
                 before sending. Code, commands, identifiers and quoted source stay as they are"
            ),
            None => "- language: use the same natural language as the latest user message for \
                     responses and all reasoning/thinking text streamed to the UI"
                .to_string(),
        };
        let mut rendered = format!(
            "Turn context:\n\
             - model: {}\n\
             - permission mode: {}\n\
             - network: {}\n\
             - cwd: {}\n\
             {}\n\
             - approval prompt: default deny; y approves once; a approves for \
             the session; d/Esc denies",
            self.model,
            mode_label(self.mode),
            network,
            self.cwd.display(),
            language,
        );
        rendered.push_str("\n\n");
        rendered.push_str(&self.operating_rules(network == "allowed"));
        if !self.project_rules.is_empty() {
            rendered.push_str("\n\nProject rules:\n");
            rendered.push_str(&render_instructions(&self.project_rules));
        }
        rendered
    }

    /// What the model must DO under this mode — not just what the mode is
    /// called. A bare `network: denied` leaves a model to flail when a fetch
    /// fails: it retries forever, or "fixes" code that was never broken. These
    /// rules name the next action for each way the sandbox can bite.
    fn operating_rules(&self, network_allowed: bool) -> String {
        let mut rules = String::from("Operating rules for this mode:\n");
        rules.push_str(
            "- For file tools, pass workspace-relative paths and use `.` for cwd itself. \
             If the user mentions the absolute cwd, translate it to `.` before calling a tool. \
             Never prefix an absolute path with `~` and never construct `~/Users/...` for \
             structured file tools. Prefer `shell_command` for git/shell one-liners; use \
             `list_files` for directories and `read_file` only for files. Do not answer a \
             task-like message with only a greeting.\n\
             - Git mutate (`git pull`/`fetch`/`commit`/`rebase`/…): under assisted/request-approval, \
             workspace `.git` is write-protected. Call `request_permissions` with \
             `filesystem=unrestricted` (and `network=true` when contacting a remote) first, \
             then run the git command. Read-only git (`status`/`diff`/`log`) does not need elevation.\n\
             - Host openers (`open` / `xdg-open` / Windows `start`): these leave the sandbox and \
             will prompt the user for approval. Prefer them when the user asks to preview a file \
             in the browser/Finder; do not claim they are blocked without having been denied.\n",
        );
        match self.mode {
            PermissionProfile::RequestApproval => rules.push_str(
                "- Permission: request-approval. Workspace edits may run; external-file \
                 intent and network use always require user approval. Expect pauses.\n",
            ),
            PermissionProfile::Assisted => rules.push_str(
                "- Permission: assisted (default). Workspace reads/writes and network tools \
                 run automatically; only irreversible, privileged, host-escape, or \
                 push/publish commands go to the user for approval.\n",
            ),
            PermissionProfile::FullAccess => rules.push_str(
                "- Permission: full-access. Commands run without approval prompts and may \
                 touch the whole machine. Take no destructive action the user did not ask for.\n",
            ),
        }
        if !network_allowed {
            rules.push_str(
                "- NETWORK IS BLOCKED. A command that fails on DNS resolution, a package \
                 registry, or a dependency download is failing because of the sandbox — \
                 that is not a bug in the code, so do not edit code in response. Call the \
                 request_permissions tool with network=true (or full_access if you also \
                 need unrestricted writes), saying what you need and why, and wait for the \
                 answer. Do not retry the same command hoping it works this time.\n",
            );
        }
        rules.push_str(
            "- If the user denies an approval, that answer is final: do NOT reach for another \
             tool, a script, or a shell trick to accomplish the same thing. Report what you \
             could not do and carry on with the rest of the task.",
        );
        rules
    }
}

fn mode_label(mode: PermissionProfile) -> &'static str {
    match mode {
        PermissionProfile::RequestApproval => "request-approval",
        PermissionProfile::Assisted => "assisted",
        PermissionProfile::FullAccess => "full-access",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_prompt_contains_agent_identity() {
        let prompt = PromptBuilder::new().build();

        assert!(prompt.contains("You are CodeLeveler"));
        assert!(prompt.contains("Read before you edit"));
    }

    #[test]
    fn base_prompt_contains_persistence_guidance() {
        let prompt = PromptBuilder::new().build();

        assert!(prompt.contains("Persist until the task is fully handled"));
        assert!(prompt.contains("do not stop just because a tool call failed"));
    }

    #[test]
    fn progress_updates_explain_evidence_impact_and_next_action() {
        let prompt = PromptBuilder::new()
            .base_instructions(Some("custom model prompt".to_string()))
            .build();

        assert!(prompt.contains("evidence"), "{prompt}");
        assert!(prompt.contains("implication"), "{prompt}");
        assert!(prompt.contains("next action"), "{prompt}");
        assert!(prompt.contains("found the root cause"), "{prompt}");
    }

    /// The full language contract — the ban on English process templates and the
    /// rewrite-before-send rule — now lives in the turn context's NAMED language
    /// line, not the base prompt. That is the load-bearing copy: it survives a
    /// model-profile override (which replaces the base) and is not paid for on
    /// turns whose language we cannot name. The base prompt no longer duplicates
    /// it.
    #[test]
    fn named_language_context_forbids_english_process_templates() {
        let prompt = PromptBuilder::new()
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("deepseek", "deepseek-chat"),
                mode: leveler_execution::PermissionProfile::Assisted,
                network_allowed: false,
                deny_network: true,
                cwd: std::path::PathBuf::from("/repo"),
                project_rules: Vec::new(),
                user_language: user_language("把这个仓库改造成生产级工具库"),
            })
            .build();

        assert!(prompt.contains("Write EVERY user-visible sentence"));
        assert!(prompt.contains("interim progress notes"));
        assert!(prompt.contains("reasoning/thinking"));
        assert!(prompt.contains("\"Now...\", \"First...\", \"Good...\", or \"Let me...\""));
        assert!(prompt.contains("wrong language"));

        // The base prompt itself no longer carries the language contract.
        assert!(!PromptBuilder::new().build().contains("Language matching"));
    }

    #[test]
    fn base_prompt_guides_javascript_package_script_commands() {
        let prompt = PromptBuilder::new().build();

        assert!(prompt.contains("Inspect the repository manifest before choosing commands"));
        assert!(prompt.contains("`npm run test -- test/foo.test.ts`"));
        assert!(prompt.contains("do not run package scripts through `npx run ...`"));
        assert!(prompt.contains("Use `npx` only for package binaries"));
        assert!(prompt.contains("If the user names an exact verification command"));
        assert!(prompt.contains("as the first verification attempt"));
        assert!(prompt.contains("do not add wrappers"));
        assert!(prompt.contains("missing from PATH"));
    }

    #[test]
    fn structural_guidance_is_opt_in() {
        let base = PromptBuilder::new().build();

        assert!(!base.contains("Before calling any tool"));
        assert!(!base.contains("do NOT declare the task complete"));

        let prompt = PromptBuilder::new().require_explicit_plan(true).build();

        assert!(prompt.contains("Before calling any tool"));
        assert!(prompt.contains("do NOT declare the task complete"));
    }

    #[test]
    fn turn_context_is_rendered_when_present() {
        let base = PromptBuilder::new().build();

        assert!(!base.contains("Turn context:"));

        let prompt = PromptBuilder::new()
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("deepseek", "deepseek-chat"),
                mode: leveler_execution::PermissionProfile::Assisted,
                network_allowed: false,
                deny_network: true,
                cwd: std::path::PathBuf::from("/repo"),
                project_rules: Vec::new(),
                user_language: None,
            })
            .build();

        assert!(prompt.contains("Turn context:"));
        assert!(prompt.contains("model: deepseek/deepseek-chat"));
        assert!(prompt.contains("permission mode: assisted"));
        assert!(prompt.contains("network: denied"));
        assert!(prompt.contains("approval prompt: default deny"));
        assert!(prompt.contains("cwd: /repo"));
        assert!(prompt.contains(
            "Co-Authored-By: CodeLeveler (deepseek/deepseek-chat) <noreply@codeleveler.com>"
        ));

        let disabled = PromptBuilder::new()
            .commit_co_author(false)
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("deepseek", "deepseek-chat"),
                mode: leveler_execution::PermissionProfile::Assisted,
                network_allowed: false,
                deny_network: true,
                cwd: std::path::PathBuf::from("/repo"),
                project_rules: Vec::new(),
                user_language: None,
            })
            .build();
        assert!(!disabled.contains("Co-Authored-By: CodeLeveler"));
    }

    #[test]
    fn turn_context_requires_workspace_relative_tool_paths() {
        let prompt = PromptBuilder::new()
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("deepseek", "deepseek-chat"),
                mode: leveler_execution::PermissionProfile::Assisted,
                network_allowed: false,
                deny_network: true,
                cwd: std::path::PathBuf::from("/Users/example/project"),
                project_rules: Vec::new(),
                user_language: None,
            })
            .build();

        assert!(prompt.contains("use `.` for cwd itself"), "{prompt}");
        assert!(prompt.contains("workspace-relative paths"), "{prompt}");
        assert!(prompt.contains("never construct `~/Users/...`"), "{prompt}");
    }

    #[test]
    fn full_access_context_renders_network_allowed() {
        let prompt = PromptBuilder::new()
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("deepseek", "deepseek-chat"),
                mode: leveler_execution::PermissionProfile::FullAccess,
                network_allowed: false,
                deny_network: false,
                cwd: std::path::PathBuf::from("/repo"),
                project_rules: Vec::new(),
                user_language: None,
            })
            .build();

        assert!(prompt.contains("network: allowed"));
    }

    /// One hardcoded prompt for every model is wrong in both directions: a weak
    /// model needs the long form with worked examples, a strong one is degraded
    /// by it. A model profile may carry its own instructions, which REPLACE the
    /// base (they are a whole prompt, not an addendum) while the turn context,
    /// project rules, and opt-in sections still apply on top.
    #[test]
    fn model_specific_instructions_replace_the_base_prompt() {
        let prompt = PromptBuilder::new()
            .base_instructions(Some("You are a terse agent.".to_string()))
            .turn_context(context(PermissionProfile::Assisted, true))
            .build();

        assert!(prompt.contains("You are a terse agent."));
        assert!(
            !prompt.contains("You are CodeLeveler"),
            "an override replaces the base prompt, it does not append to it"
        );
        assert!(
            prompt.contains("Turn context:"),
            "the turn context still applies on top of an overridden base"
        );
        assert!(prompt.contains("reasoning/thinking text"));
        assert!(prompt.contains("latest user message"));
    }

    #[test]
    fn no_override_keeps_the_default_base_prompt() {
        let prompt = PromptBuilder::new().base_instructions(None).build();

        assert!(prompt.contains("You are CodeLeveler"));
    }

    /// The base prompt now lives in prompts/base.md, not a Rust string literal.
    /// Guard the include: an empty or truncated file must not ship silently.
    #[test]
    fn the_base_prompt_is_loaded_from_the_markdown_file() {
        assert!(
            BASE_PROMPT.len() > 500,
            "prompts/base.md looks empty or truncated"
        );
        assert!(BASE_PROMPT.contains("You are CodeLeveler"));
    }

    /// Analysis/review answers must not promote "tests passed" into unearned
    /// performance or "no regression" claims (evidence discipline).
    #[test]
    fn base_prompt_requires_evidence_layers_for_analysis_claims() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("Evidence discipline"),
            "must name the analysis evidence rules"
        );
        assert!(
            prompt.contains("not measured") || prompt.contains("Not measured"),
            "must force unmeasured benefits to be labeled"
        );
        assert!(
            prompt.contains("first true deep copy") || prompt.contains("deep copy"),
            "must require tracing Arc/clone claims to the first deep copy"
        );
        assert!(
            prompt.contains("do **not** write \"no regression\"")
                || prompt.contains("no regression"),
            "must forbid overclaiming from default test green"
        );
    }

    /// The default guidance tells the model to GO DEEP, which is right for
    /// analysis but wrong for reporting an edit — it produces a final message
    /// pasting whole before/after bodies the user can already see in the diff.
    #[test]
    fn reporting_a_code_change_is_compact_and_never_pastes_the_diff() {
        let prompt = PromptBuilder::new().build();

        assert!(
            prompt.contains("size the message to the change"),
            "the report must scale with the edit"
        );
        assert!(
            prompt.contains("NEVER paste before/after pairs"),
            "the user already has the diff"
        );
        assert!(
            prompt.contains("does not apply to analysis"),
            "must not muzzle explanation answers"
        );
    }

    /// We inject nested AGENTS.md blocks mid-transcript but never told the model
    /// how they compose. Two holes: a deep rule silently loses to a root rule (or
    /// vice versa, unpredictably), and — the security one — a rules FILE can tell
    /// the model to ignore the user, because nothing established who outranks whom.
    #[test]
    fn project_rules_have_a_scope_and_a_precedence_order() {
        let prompt = PromptBuilder::new().build();

        assert!(
            prompt.contains("entire directory tree rooted at"),
            "scope must be stated"
        );
        assert!(
            prompt.contains("more deeply nested"),
            "conflicts need a winner"
        );
        assert!(
            prompt.contains("take precedence over any project rule"),
            "a rules file must not be able to outrank the user"
        );
    }

    /// Running the whole suite first is slow and, worse, surfaces pre-existing
    /// failures the model then tries to "fix" — derailing the actual task. And a
    /// repo with no tests must not grow a test framework the user never asked for.
    #[test]
    fn verification_narrows_before_it_widens_and_ignores_unrelated_failures() {
        let prompt = PromptBuilder::new().build();

        assert!(
            prompt.contains("narrowest check that exercises your change"),
            "must verify the change itself before the whole suite"
        );
        assert!(
            prompt.contains("do NOT fix them"),
            "pre-existing failures are not this task"
        );
        assert!(
            prompt.contains("no tests at all"),
            "must not bolt a test framework onto a repo that has none"
        );
    }

    /// "Keep a short checklist" names the tool but sets no bar, so a weak model
    /// writes a plan whose steps merely restate the goal ("1. Build the CLI
    /// tool") — pure overhead that verifies nothing. The prompt must show the
    /// difference and forbid the degenerate cases.
    #[test]
    fn plan_guidance_sets_a_quality_bar_not_just_a_tool_name() {
        let prompt = PromptBuilder::new().build();

        assert!(
            prompt.contains("never write a single-step plan"),
            "a one-step plan is pure overhead"
        );
        assert!(
            prompt.contains("restate the goal"),
            "must name the degenerate plan"
        );
        assert!(prompt.contains("Bad plan"), "needs a contrasting example");
        assert!(prompt.contains("Good plan"), "needs a contrasting example");
        assert!(
            prompt.contains("independently verifiable"),
            "must state what a step actually is"
        );
        assert!(
            prompt.contains("do not repeat the plan back"),
            "the UI already renders it — repeating it wastes the turn"
        );
    }

    /// Strict status discipline. Without it a weak model batch-completes
    /// everything at the end (or jumps a step straight to completed), so the
    /// rendered plan lies about progress; and it keeps coding against a plan
    /// that no longer matches reality instead of updating it first.
    #[test]
    fn plan_status_discipline_forbids_jumps_batches_and_stale_plans() {
        let prompt = PromptBuilder::new().build();

        assert!(
            prompt.contains("never jump pending to completed"),
            "a step must pass through in_progress"
        );
        assert!(
            prompt.contains("never batch-complete"),
            "steps must be marked as they actually finish"
        );
        assert!(
            prompt.contains("BEFORE continuing"),
            "a changed understanding updates the plan first, then the work resumes"
        );
        assert!(
            prompt.contains("dangling in_progress"),
            "the task must not end with an unfinished-looking plan"
        );
    }

    /// The Explicit planning gate (weak models) told the model to write a prose
    /// plan — invisible to the UI and immediately stale. Multi-step plans must
    /// route into update_plan; single-step work must not grow a plan at all.
    #[test]
    fn explicit_plan_gate_routes_multi_step_plans_into_update_plan() {
        let base = PromptBuilder::new().build();
        assert!(
            !base.contains("register that plan with the update_plan tool"),
            "the gate stays opt-in"
        );

        let prompt = PromptBuilder::new().require_explicit_plan(true).build();
        assert!(
            prompt.contains("update_plan")
                && prompt.contains("first substantive action must be update_plan"),
            "multi-step work must route into the tracked checklist, not prose: {prompt}"
        );
        assert!(
            prompt.contains("skip update_plan") || prompt.contains("single action covers"),
            "must not force a plan onto single-step work"
        );
    }

    fn context(mode: PermissionProfile, network_allowed: bool) -> TurnContext {
        TurnContext {
            model: leveler_model::ModelRef::new("mock", "m"),
            mode,
            network_allowed,
            deny_network: !network_allowed,
            cwd: std::path::PathBuf::from("/repo"),
            project_rules: Vec::new(),
            user_language: None,
        }
    }

    /// Stating `network: denied` tells the model the state but not the action.
    /// A model that hits a dependency-download failure then "fixes" the code,
    /// or retries the same command forever. The prompt must name the escape.
    #[test]
    fn blocked_network_tells_the_model_what_to_do_about_it() {
        let prompt = PromptBuilder::new()
            .turn_context(context(PermissionProfile::Assisted, false))
            .build();

        assert!(
            prompt.contains("request_permissions"),
            "names the escape tool"
        );
        assert!(
            prompt.contains("not a bug in the code"),
            "a sandbox failure must not be mistaken for a code defect"
        );
        assert!(
            prompt.contains("Do not retry the same command"),
            "must forbid the retry loop"
        );
    }

    /// The security rule: a denied approval is final. Without this a model
    /// routes around the user (denied `run_command` → same thing via a script).
    #[test]
    fn a_denied_approval_must_not_be_circumvented() {
        let prompt = PromptBuilder::new()
            .turn_context(context(PermissionProfile::Assisted, false))
            .build();

        assert!(
            prompt.contains("do NOT reach for another tool"),
            "denial must not be routed around"
        );
    }

    /// Request-approval profile must state that network/external work needs a yes.
    #[test]
    fn request_approval_mode_requires_user_yes_for_network() {
        let prompt = PromptBuilder::new()
            .turn_context(context(PermissionProfile::RequestApproval, false))
            .build();

        assert!(prompt.contains("request-approval"));
        assert!(
            prompt.contains("require user approval") || prompt.contains("always require"),
            "request-approval must mention mandatory approval for external/network work"
        );
    }

    #[test]
    fn operating_rules_forbid_empty_greeting_and_steer_shell_and_list_files() {
        let prompt = PromptBuilder::new()
            .turn_context(context(PermissionProfile::Assisted, false))
            .build();
        assert!(
            prompt.contains("shell_command"),
            "must prefer shell_command for git/shell: {prompt}"
        );
        assert!(
            prompt.contains("list_files"),
            "must steer directories to list_files: {prompt}"
        );
        assert!(
            prompt.contains("greeting") || prompt.contains("task-like"),
            "must ban greeting-only replies to tasks: {prompt}"
        );
        assert!(
            prompt.contains("filesystem=unrestricted")
                && (prompt.contains("git pull") || prompt.contains("Git mutate")),
            "must require FS elevation before git mutate: {prompt}"
        );
    }

    #[test]
    fn base_prompt_enforces_concise_presentation() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("Presenting your work and final message"),
            "must include presentation guidance: {prompt}"
        );
        assert!(
            prompt.contains("greetings") || prompt.contains("casual conversation"),
            "must teach casual/greeting brevity: {prompt}"
        );
        assert!(
            prompt.contains("no previous context") || prompt.contains("Same-session"),
            "must teach follow-up context use: {prompt}"
        );
        assert!(
            !prompt.contains("stacked \"analysis done"),
            "old closeout-filler wording should be gone"
        );
        assert!(
            prompt.contains("Soft follow-up tip"),
            "must teach optional friendly tip (not process closeout): {prompt}"
        );
        assert!(
            prompt.contains("at most one short tip line")
                || prompt.contains("at most one soft tip"),
            "must cap tips to one line: {prompt}"
        );
        assert!(
            prompt.contains("Do not tip when") || prompt.contains("don't invent a multi-item"),
            "must forbid invented roadmaps as tips: {prompt}"
        );
        assert!(
            prompt.contains("纯信息查询") || prompt.contains("process closeout"),
            "must keep process-closeout examples banned: {prompt}"
        );
    }

    #[test]
    fn base_prompt_bans_generic_greeting_on_tasks() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("Never reply with only a generic greeting")
                || prompt.contains("generic greeting"),
            "base prompt must ban empty greetings on tasks"
        );
        assert!(prompt.contains("shell_command"));
        assert!(prompt.contains("list_files"));
        assert!(
            prompt.contains("request_user_input"),
            "base prompt must advertise the primary clarification tool"
        );
        // The git-mutate elevation rule depends on the permission mode, so it
        // lives in the turn context (see operating_rules), not the base prompt.
        assert!(
            !prompt.contains("filesystem=unrestricted"),
            "git elevation is a turn-context rule, not a base-prompt one"
        );
        assert!(
            prompt.contains("SKILL TURN INJECTION")
                || prompt.contains("load_skill")
                || prompt.contains("progressive disclosure"),
            "base prompt must include skills how-to-use: {prompt}"
        );
        assert!(
            prompt.contains("$name") || prompt.contains("$skill") || prompt.contains("`$name`"),
            "base prompt must mention $name skill naming"
        );
    }

    /// Full access grants no network prompt, so the blocked-network rules must not fire.
    #[test]
    fn full_access_does_not_emit_the_blocked_network_rules() {
        let prompt = PromptBuilder::new()
            .turn_context(context(PermissionProfile::FullAccess, false))
            .build();

        assert!(
            !prompt.contains("NETWORK IS BLOCKED"),
            "full-access must not emit the blocked-network operating rule: {prompt}"
        );
        assert!(
            prompt.contains("full-access") || prompt.contains("destructive"),
            "full-access operating rules must be present"
        );
    }

    #[test]
    fn turn_context_renders_project_rules() {
        let prompt = PromptBuilder::new()
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("mock", "m"),
                mode: leveler_execution::PermissionProfile::Assisted,
                network_allowed: false,
                deny_network: true,
                cwd: std::path::PathBuf::from("/repo"),
                project_rules: vec![ProjectInstruction {
                    source: "src/AGENTS.md".to_string(),
                    content: "Prefer small modules.".to_string(),
                }],
                user_language: None,
            })
            .build();

        assert!(prompt.contains("Project rules:"));
        assert!(prompt.contains("--- from src/AGENTS.md ---"));
        assert!(prompt.contains("Prefer small modules."));
    }
    #[test]
    fn a_chinese_request_names_the_language_instead_of_asking_the_model_to_infer_it() {
        // "Use the same natural language as the latest user message" makes the
        // model infer the language and then police itself against it. Measured
        // over three real sessions, deepseek-v4-pro ignored it for 49% of its
        // user-visible messages — every one of them an interim note ("Now let me
        // ...") streamed straight to the TUI, in English, to a user writing
        // Chinese. Resolving the language in code and NAMING it is the reliability
        // move: a weak model follows "write in Chinese" reliably.
        let prompt = PromptBuilder::new()
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("deepseek", "deepseek-chat"),
                mode: leveler_execution::PermissionProfile::Assisted,
                network_allowed: false,
                deny_network: true,
                cwd: std::path::PathBuf::from("/repo"),
                project_rules: Vec::new(),
                user_language: user_language("把这个仓库改造成生产级的 Go 工具库"),
            })
            .build();

        assert!(
            prompt.contains("- language: the user writes Chinese (中文)"),
            "the turn context must name the language: {prompt}"
        );
    }

    #[test]
    fn an_english_request_keeps_the_generic_mirroring_rule() {
        let prompt = PromptBuilder::new()
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("deepseek", "deepseek-chat"),
                mode: leveler_execution::PermissionProfile::Assisted,
                network_allowed: false,
                deny_network: true,
                cwd: std::path::PathBuf::from("/repo"),
                project_rules: Vec::new(),
                user_language: user_language("make this repo production ready"),
            })
            .build();

        assert!(
            !prompt.contains("- language: the user writes"),
            "an English request must not be told to answer in Chinese: {prompt}"
        );
        assert!(
            prompt.contains("- language: use the same natural language as the latest user message"),
            "unnamed languages still get the generic rule: {prompt}"
        );
    }

    #[test]
    fn code_and_paths_do_not_decide_the_language() {
        // An English request that quotes Chinese source must not flip to Chinese,
        // and a Chinese request full of code must still read as Chinese.
        assert_eq!(
            user_language("rename the `软件名称` field to `title` in swcr.go"),
            None
        );
        assert!(user_language("把 `CodeFinder.find()` 的返回值改成 `([]string, error)`").is_some());
    }

    #[test]
    fn core_system_prefix_is_byte_stable_across_assemblies() {
        // Cache-stable prefix: same builder options must yield identical system text
        // when turn context is absent (task contracts never land in this path).
        let a = PromptBuilder::new().require_explicit_plan(true).build();
        let b = PromptBuilder::new().require_explicit_plan(true).build();
        assert_eq!(a, b);
        assert!(!a.contains("Request:"));
        assert!(!a.contains("Constraints:"));
        assert!(a.contains("current step k/n") || a.contains("Progress narration"));
    }

    #[test]
    fn memory_index_is_stable_and_excludes_bodies() {
        let index = "1. [pref] Prefer workspace-write\n2. [style] Use tables in reviews";
        let a = PromptBuilder::new().memory_index(index).build();
        let b = PromptBuilder::new().memory_index(index).build();
        assert_eq!(a, b);
        assert!(a.contains("Project memory index"));
        assert!(a.contains("[pref] Prefer workspace-write"));
        assert!(!a.contains("PermissionProfile")); // body text must not appear
    }
}
