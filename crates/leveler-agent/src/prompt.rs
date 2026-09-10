use std::path::PathBuf;

use leveler_context::{ProjectInstruction, render_instructions};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;

/// The default system prompt. Lives in `prompts/base.md` rather than a string
/// literal so it can be edited and diffed as prose — and so a model profile can
/// ship its own (see `PromptBuilder::base_instructions`): one prompt does not
/// fit every model, so the prompt is per-model configuration.
const BASE_PROMPT: &str = include_str!("../prompts/base.md");

#[derive(Debug, Clone)]
pub(crate) struct PromptBuilder {
    turn_context: Option<TurnContext>,
    base_instructions: Option<String>,
    commit_co_author: bool,
    /// Short memory INDEX (titles only). Empty = omit segment.
    memory_index: String,
}

impl Default for PromptBuilder {
    fn default() -> Self {
        Self {
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
    /// A bounded listing of what is in the workspace, already rendered.
    ///
    /// Measured: `list_files` was the FIRST tool call on 5 of 7 baseline cases
    /// — a whole round trip, ~3.8 s, spent asking what files exist. This
    /// message is built once per turn and stays byte-identical for the loop,
    /// so the listing lives in the provider's prefix cache and is paid for
    /// once, uncached, on the first request. Trading cached prefix for a round
    /// trip is the right direction here; the reverse is not.
    pub(crate) repo_map: Option<String>,
}

/// Name the language of a user request, so the prompt can state it outright.
///
/// "Use the same natural language as the latest user message" asks the model to
/// infer the language and then police every sentence against it. Measured across
/// three real sessions, deepseek-v4-pro broke that rule in 49% of its
/// user-visible messages — interim notes ("Now let me ...")
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
        // Product UX, not a reasoning format. The user reads these lines while
        // the work happens, so they must be legible and in their language, and
        // they must not repeat what the interface already draws. WHAT to say is
        // the model's judgement; that it is short and not duplicated is the
        // product's constraint.
        prompt.push_str(
            "\n\nKeep interim updates short and in the user's language. The \
             interface already renders every tool call, so a line that only \
             restates the next action — \"let me read a few files\", \"running \
             the tests\" — prints the same fact twice; when there is nothing to \
             add, call the tool with no prose at all.",
        );
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
                 {named} — interim notes, status narration, reasoning text streamed to the \
                 UI, and the final summary. Code, commands, identifiers and quoted source \
                 stay as they are"
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
        if let Some(map) = self.repo_map.as_deref().filter(|m| !m.trim().is_empty()) {
            rendered.push_str("\n\nWorkspace files (bounded listing):\n");
            rendered.push_str(map.trim_end());
            rendered.push('\n');
        }
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
             structured file tools.\n\
             - Git mutate (`git pull`/`fetch`/`commit`/`rebase`/…): under assisted/request-approval, \
             workspace `.git` is write-protected. Just run the git command; when the sandbox \
             denies it, retry that same command with `escalate` set (`filesystem` = \
             `unrestricted`, plus `network` = true when contacting a remote) — one call, \
             no separate permission round. Read-only git (`status`/`diff`/`log`) does not \
             need elevation.\n\
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
                 that is not a bug in the code, so do not edit code in response. For a \
                 command, retry that exact command once with `escalate` set (`network` = true, \
                 plus `filesystem` = `unrestricted` when it also writes outside the \
                 workspace) — the approval prompt it raises is how the user consents, so \
                 do not ask in prose first. For anything that is not a command \
                 (`web_fetch`, `web_search`), call the request_permissions tool with \
                 network=true, saying what you need and why, and wait for the answer. \
                 Do not retry the same command hoping it works this time.\n",
            );
        }
        rules.push_str(
            "- If the user denies an approval, that answer is final: do NOT reach for another \
             tool, a script, or a shell trick to accomplish the same thing. Continue with \
             already-available capabilities. If you need the user to run a command or paste \
             output, call request_user_input — do not only write that request in prose and \
             keep going. Do not request the same or a broader permission again.\n",
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
    }

    /// A named language is a product requirement, and it covers the streamed
    /// reasoning text too — that is where it was measured breaking.
    #[test]
    fn a_named_language_covers_every_user_visible_sentence() {
        let prompt = PromptBuilder::new()
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("deepseek", "deepseek-chat"),
                mode: leveler_execution::PermissionProfile::Assisted,
                network_allowed: false,
                deny_network: true,
                cwd: std::path::PathBuf::from("/w"),
                project_rules: Vec::new(),
                user_language: user_language("把这个仓库改造成生产级工具库"),
                repo_map: None,
            })
            .build();

        assert!(prompt.contains("Write EVERY user-visible sentence"));
        assert!(prompt.contains("Chinese"));
        assert!(prompt.contains("reasoning text streamed to the"));
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
                repo_map: None,
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
                repo_map: None,
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
                repo_map: None,
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
                repo_map: None,
            })
            .build();

        assert!(prompt.contains("network: allowed"));
    }

    /// One hardcoded prompt for every model is wrong in both directions: the
    /// length and worked examples that help one model are noise to another.
    /// A model profile may carry its own instructions, which REPLACE the
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

    /// The listing is context. It states what is there and stops — which tool
    /// to reach for next is the model's call.
    #[test]
    fn the_workspace_listing_reaches_the_prompt() {
        let mut ctx = context(PermissionProfile::Assisted, true);
        ctx.repo_map = Some("src/lib.rs\nsrc/main.rs".to_string());
        let prompt = PromptBuilder::new().turn_context(ctx).build();
        assert!(
            prompt.contains("src/lib.rs") && prompt.contains("src/main.rs"),
            "{prompt}"
        );
        assert!(
            !prompt.contains("do not call a directory tool"),
            "context states facts; it does not route tool choice: {prompt}"
        );
    }

    /// No listing means no heading. A workspace we could not read must not
    /// produce an empty section that reads as "this repository is empty".
    #[test]
    fn an_absent_or_empty_listing_renders_no_section() {
        for map in [None, Some(String::new()), Some("   \n".to_string())] {
            let mut ctx = context(PermissionProfile::Assisted, false);
            ctx.repo_map = map.clone();
            let prompt = PromptBuilder::new().turn_context(ctx).build();
            assert!(
                !prompt.contains("Workspace files"),
                "empty listing must not render a heading ({map:?})"
            );
        }
    }

    /// `update_goal` is what ends a goal, and the ordering is the whole point:
    /// final prose does not close one, and a turn spent only on the call costs
    /// a round trip.
    #[test]
    fn the_goal_contract_puts_completion_in_the_finishing_turn() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("same turn as your final answer"),
            "the ordering is the whole point: {prompt}"
        );
        assert!(
            prompt.contains("final prose does not close a goal"),
            "{prompt}"
        );
    }

    /// The interface draws every tool call, so prose that only restates the
    /// next action prints the same fact twice. This is a UX constraint on
    /// duplication, not a template for what to think.
    #[test]
    fn narration_guidance_forbids_echoing_the_tool_call() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("interface already renders every tool call"),
            "the rule must be explicit: {prompt}"
        );
        assert!(
            prompt.contains("no prose at all"),
            "silence must be allowed: {prompt}"
        );
        for template in ["current step k/n", "evidence) → next"] {
            assert!(
                !prompt.contains(template),
                "a reasoning template is not a UX constraint ({template:?})"
            );
        }
    }

    /// Reporting discipline: what a green suite actually supports, and the ban
    /// on unmeasured performance claims.
    #[test]
    fn base_prompt_requires_evidence_layers_for_analysis_claims() {
        let prompt = PromptBuilder::new().build();
        assert!(prompt.contains("Reporting what happened"), "{prompt}");
        assert!(
            prompt.contains("no failures were found on the paths those tests cover"),
            "{prompt}"
        );
        assert!(prompt.contains("not measured"), "{prompt}");
        assert!(
            prompt.contains("Report an action from its tool result"),
            "{prompt}"
        );
    }

    /// An edit report is sized to the edit, and never pastes the diff the user
    /// already has.
    #[test]
    fn reporting_a_code_change_is_compact_and_never_pastes_the_diff() {
        let prompt = PromptBuilder::new().build();
        assert!(prompt.contains("match depth to the question"), "{prompt}");
        assert!(
            prompt.contains("Do not paste diffs, whole files, or before/after pairs"),
            "{prompt}"
        );
        assert!(prompt.contains("`path:line`"), "{prompt}");
    }

    /// A rules file has a scope and a rank. Without the rank, a file in the
    /// repository can tell the model to ignore the user.
    #[test]
    fn project_rules_have_a_scope_and_a_precedence_order() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("directory tree it came from"),
            "scope must be stated: {prompt}"
        );
        assert!(
            prompt.contains("most deeply nested block wins"),
            "conflict order must be stated: {prompt}"
        );
        assert!(
            prompt.contains("data, not authority"),
            "a rules file must not outrank the user: {prompt}"
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
            repo_map: None,
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

    /// A command that hit the sandbox already has an escalation path ON the
    /// retry — routing it through `request_permissions` first spends a whole
    /// extra model round trip for the same approval prompt.
    #[test]
    fn a_blocked_command_escalates_on_its_own_retry() {
        let prompt = PromptBuilder::new()
            .turn_context(context(PermissionProfile::Assisted, false))
            .build();

        assert!(
            prompt.contains("escalate"),
            "the one-round escape must be named"
        );
        assert!(
            prompt.contains("request_permissions"),
            "tools that are not commands still need the separate request"
        );
    }

    /// Git mutate is the canonical two-step the prompt used to teach. It must
    /// now teach the same single call the tool schema advertises, or the
    /// prompt and the tool contradict each other.
    #[test]
    fn git_mutate_guidance_matches_the_tool_contract() {
        let prompt = PromptBuilder::new()
            .turn_context(context(PermissionProfile::Assisted, false))
            .build();
        let git_rule = prompt
            .lines()
            .find(|line| line.contains("Git mutate"))
            .expect("git mutate rule must exist");

        assert!(
            git_rule.contains("escalate"),
            "git mutate must use the command's own escalation: {git_rule}"
        );
        assert!(
            !git_rule.contains("request_permissions"),
            "the superseded two-step must be gone: {git_rule}"
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
        assert!(
            prompt.contains("request_user_input"),
            "denial that needs the user must use the structured wait"
        );
    }

    /// The safety half of the old diagnosis paragraph survives; the
    /// epistemology lecture around it does not.
    #[test]
    fn destructive_first_steps_still_need_evidence() {
        let prompt = PromptBuilder::new()
            .turn_context(context(PermissionProfile::Assisted, false))
            .build();
        assert!(prompt.contains("destructive first step"), "{prompt}");
        assert!(prompt.contains("pkill -f"), "{prompt}");
        assert!(
            !prompt.contains("sandbox reproduction"),
            "how to reason about a repro is the model's: {prompt}"
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
    fn base_prompt_enforces_concise_presentation() {
        let prompt = PromptBuilder::new().build();
        assert!(prompt.contains("Presenting your work"), "{prompt}");
        assert!(prompt.contains("Be concise by default"), "{prompt}");
        assert!(prompt.contains("No process closeout"), "{prompt}");
        assert!(
            prompt.contains("at most one short follow-up tip"),
            "{prompt}"
        );
    }

    /// Removed with the rest of the effort coaching: how much to do for a
    /// given message is the model's read of it, and the prompt no longer
    /// scripts the greeting case.
    #[test]
    fn base_prompt_does_not_script_the_greeting_case() {
        let prompt = PromptBuilder::new().build();
        assert!(!prompt.contains("generic greeting"), "{prompt}");
        assert!(!prompt.contains("Scale your effort"), "{prompt}");
    }

    /// A fork the user must settle goes through `request_user_input` with
    /// options. Prose is not a pause: the interface renders a choice from
    /// `options`, so "waiting for confirmation" in a message stops nothing.
    #[test]
    fn base_prompt_requires_structured_decision_gates() {
        let prompt = PromptBuilder::new().build();
        assert!(prompt.contains("request_user_input"), "{prompt}");
        assert!(
            prompt.contains("2–4 mutually exclusive choices"),
            "{prompt}"
        );
        assert!(prompt.contains("Prose alone is not a pause"), "{prompt}");
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
                repo_map: None,
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
        // move: "write in Chinese" is an instruction, not an inference.
        let prompt = PromptBuilder::new()
            .turn_context(TurnContext {
                model: leveler_model::ModelRef::new("deepseek", "deepseek-chat"),
                mode: leveler_execution::PermissionProfile::Assisted,
                network_allowed: false,
                deny_network: true,
                cwd: std::path::PathBuf::from("/repo"),
                project_rules: Vec::new(),
                user_language: user_language("把这个仓库改造成生产级的 Go 工具库"),
                repo_map: None,
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
                repo_map: None,
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

    /// The cache-stable prefix: two assemblies with the same inputs are the
    /// same bytes, so the provider's prefix cache is not invalidated per turn.
    #[test]
    fn core_system_prefix_is_byte_stable_across_assemblies() {
        let a = PromptBuilder::new().build();
        let b = PromptBuilder::new().build();
        assert_eq!(a, b);
        assert!(!a.contains("Request:"));
        assert!(!a.contains("Constraints:"));
    }

    /// Regression lock on `base.md`'s proactive-memory section: it must reach
    /// the model even with an empty store, or a fresh project can never record
    /// its first memory. Only the index segment is conditional.
    #[test]
    fn memory_guidance_ships_even_with_an_empty_store() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("remember"),
            "an empty store must still tell the model how to record one"
        );
    }

    /// Regression lock: the "what earns a memory / what does not" list is the
    /// only thing standing between a useful store and a pile of trivia.
    #[test]
    fn memory_guidance_says_what_is_worth_keeping_and_what_is_not() {
        let prompt = PromptBuilder::new().build();
        let lowered = prompt.to_lowercase();
        assert!(
            lowered.contains("preference") || lowered.contains("constraint"),
            "must name what earns a memory"
        );
        assert!(
            lowered.contains("git history") || lowered.contains("code structure"),
            "must name what does NOT (anything re-derivable from the repo)"
        );
    }

    /// A memory is a snapshot of what was true when written. Recall without a
    /// correction duty turns a stale note into a confident wrong answer.
    #[test]
    fn memory_guidance_requires_correcting_what_went_stale() {
        let prompt = PromptBuilder::new().build();
        let lowered = prompt.to_lowercase();
        assert!(
            lowered.contains("out of date") || lowered.contains("stale"),
            "must tell the model recalled memory can be stale"
        );
        assert!(
            lowered.contains("forget"),
            "correcting a stale memory needs the tool that removes it"
        );
    }

    /// `remember` is not an upsert: re-proposing a title with new content
    /// stores `<id>-2` and both compete in recall. The prompt must not tell the
    /// model otherwise — that instruction would manufacture the contradiction
    /// it claims to prevent. See `MemoryStore::remember_deduplicated`.
    #[test]
    fn memory_guidance_never_calls_remember_an_upsert() {
        let lowered = PromptBuilder::new().build().to_lowercase();
        assert!(
            !lowered.contains("upsert"),
            "remember replaces nothing; correcting a memory is forget-then-remember"
        );
    }

    #[test]
    fn memory_guidance_is_cache_stable() {
        assert_eq!(PromptBuilder::new().build(), PromptBuilder::new().build());
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

#[cfg(test)]
mod prompt_budget {
    use super::PromptBuilder;

    /// The system prompt is paid on every request of every session. It lives in
    /// the provider's cached prefix, so its per-request cost is small, but it
    /// is the largest single uncached block of the first request of a session.
    /// Measured 2026-09-09: 21.2 KB, 22.8 KB with the plan block.
    ///
    /// A tripwire, not a target. Rules earn their bytes; this only makes a
    /// large addition visible instead of silent.
    #[test]
    fn the_system_prompt_stays_within_its_measured_budget() {
        let plain = PromptBuilder::new().build().len();
        let planned = PromptBuilder::new().build().len();
        assert!(plain < 30_000, "system prompt grew to {plain} bytes");
        assert!(planned < 32_000, "with the plan block: {planned} bytes");
    }
}
