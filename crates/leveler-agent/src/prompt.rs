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
        // The UI already draws every tool call ("reading 4 files"). Prose that
        // only restates the next action therefore prints the same fact twice
        // and pushes the actual work down the screen. Silence is a legitimate
        // answer here — the runtime narrates the action, the model narrates
        // the reasoning.
        prompt.push_str(
            "\n\nDo not narrate what the tool call already shows. The interface \
             renders every tool call as it happens, so a line that only restates \
             the next action — \"let me read a few files\", \"searching again\", \
             \"running the tests\" — says the same thing twice and adds nothing. \
             Say something only when you have a purpose, an observation, what it \
             implies, or a reason for the next step: \"the entry point already \
             moved to the new router, so I am checking what still references the \
             old one\". When there is nothing to add, call the tool with no prose \
             at all — that is better than an empty line of narration. One \
             sentence is enough; this is not a request for longer explanations.",
        );
        if self.require_explicit_plan {
            prompt.push_str(
                "\n\nIf this is multi-step work (several independently checkable \
                 pieces, multi-file changes, or migrate/architecture work), call \
                 update_plan with one in_progress step and the rest pending \
                 (statuses: pending/in_progress/completed) — not a prose checklist \
                 alone — before you start changing files. Locating and reading \
                 code needs no plan: \
                 investigate as long as the evidence still changes your \
                 understanding, and write the plan once you know what the work \
                 actually is. If a single action covers the request, skip \
                 update_plan and just do the task.",
            );
            // A plan is only worth showing if it tracks the work. Created once
            // and never touched again, it leaves the user watching "step 1/8 in
            // progress" while the agent is building step 5 — and the runtime
            // cannot fix that for them: whether a step is done is a judgement
            // only the model can make.
            prompt.push_str(
                "\n\nWhen an active plan exists, keep it synchronized with your actual \
                 work. At most one step is in_progress. When you finish the current \
                 step and move on to another, call update_plan at that transition to \
                 mark the finished step completed and the next one in_progress — not \
                 several steps later, and not as one batch at the end. Do not call it \
                 between the tool calls inside a single step: a step that needs a \
                 read, two edits and a test run stays in_progress for all of them. If \
                 you finished several steps in one stretch, mark them all completed in \
                 one call. Never mark a step completed just to make the checklist look \
                 current — update it only when you believe that step's work is \
                 actually done, and if you are unsure, leave it as it is.",
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
        if let Some(map) = self.repo_map.as_deref().filter(|m| !m.trim().is_empty()) {
            rendered.push_str(
                "\n\nWorkspace files (bounded listing; do not call a directory tool just to \
                 learn what exists here — read or search what you need):\n",
            );
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
             structured file tools. Prefer `shell_command` for git/shell one-liners; use \
             `list_files` for directories and `read_file` only for files. Do not answer a \
             task-like message with only a greeting.\n\
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
        rules.push_str(
            "- Diagnosis: separate confirmed facts from speculation. A sandbox reproduction \
             is not a host reproduction. A CONNECT 502 is not proof the sandbox blocked \
             the network unless a policy denial said so. A SQLite readonly error is not \
             proof of a leftover process lock. Do not recommend destructive first steps \
             (pkill -f, deleting WAL/SHM, chmod of large trees) without direct evidence; \
             inspect safely first, then ask the user to verify host state you cannot see.",
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
                repo_map: None,
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

        assert!(!base.contains("before you start changing files"));
        assert!(!base.contains("do NOT declare the task complete"));

        let prompt = PromptBuilder::new().require_explicit_plan(true).build();

        assert!(prompt.contains("before you start changing files"));
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

    /// C2.3B §30 A/B/C — navigation discipline is HARNESS baseline behavior:
    /// it ships in `prompts/base.md`, so every production request carries it,
    /// and every provider gets the same principles. A model profile that
    /// C2.3B §2/§6 — broader reads must stay legitimate. The guidance may not
    /// forbid whole-file reads or argue from token cost: the goal is evidence
    /// C2.3B §4 — a known location must not cost a ceremonial search. The
    /// guidance has to state the KNOWN case, or "search first" degrades into
    /// `list_files` was the FIRST tool call on 5 of 7 baseline cases: a whole
    /// round trip spent asking what exists. The listing rides in the system
    /// prompt, which is built once per turn and stays byte-identical for the
    /// loop, so it is paid for once and served from the prefix cache after.
    #[test]
    fn the_workspace_listing_reaches_the_prompt_and_says_what_it_is_for() {
        let mut ctx = context(PermissionProfile::Assisted, false);
        ctx.repo_map = Some("src/lib.rs\nsrc/main.rs".to_string());
        let prompt = PromptBuilder::new().turn_context(ctx).build();
        assert!(
            prompt.contains("src/lib.rs") && prompt.contains("src/main.rs"),
            "{prompt}"
        );
        assert!(
            prompt.contains("do not call a directory tool just to learn what exists"),
            "the listing has to say why it is there: {prompt}"
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

    /// A goal that ends correctly but in two turns costs a whole extra round
    /// trip — measured at ~3.8 s, and it fired on 5 of 7 baseline cases. The
    /// closeout nudge recovers it, but recovery is not the normal path. The
    /// prompt used to call `update_goal` "silent bookkeeping", which reads as
    /// a formality to do afterwards rather than the act that ends the goal.
    #[test]
    fn the_goal_contract_puts_completion_in_the_finishing_turn() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("SAME turn as your final answer"),
            "the ordering is the whole point: {prompt}"
        );
        assert!(
            prompt.contains("Final prose does not close a goal"),
            "what does NOT end a goal has to be said: {prompt}"
        );
        assert!(
            !prompt.contains("silent bookkeeping"),
            "that framing is what made it look optional: {prompt}"
        );
        assert!(
            prompt.contains("never narrate process state"),
            "keeping the call invisible to the user must survive: {prompt}"
        );
    }

    /// The UI already draws every tool call, so prose that only restates the
    /// next action prints the same fact twice. The contract has to say that
    /// silence is allowed — otherwise the model fills every gap with
    /// "let me read a few files" above a row that already says so.
    #[test]
    fn narration_guidance_forbids_echoing_the_tool_call() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("Do not narrate what the tool call already shows"),
            "the rule must be explicit: {prompt}"
        );
        assert!(
            prompt.contains("call the tool with no prose"),
            "saying nothing has to be an allowed answer: {prompt}"
        );
        assert!(
            prompt.contains("purpose") && prompt.contains("implies"),
            "what a narration should add must be named: {prompt}"
        );
        assert!(
            prompt.contains("One sentence is enough"),
            "and it must not invite longer prose: {prompt}"
        );
    }

    /// A plan created once and never touched again is worse than no plan: the
    /// user watches "step 1/8 in progress" while the agent builds step 5. The
    /// prompt has to name WHEN to synchronize, because the runtime cannot —
    /// whether a step is done is a judgement only the model can make.
    #[test]
    fn plan_guidance_says_when_to_synchronize_not_just_to_revise() {
        let prompt = PromptBuilder::new().require_explicit_plan(true).build();
        assert!(
            prompt.contains("keep it synchronized"),
            "the standing obligation must be explicit: {prompt}"
        );
        assert!(
            prompt.contains("at that transition"),
            "the moment matters, not just the act: {prompt}"
        );
        assert!(
            prompt.contains("At most one step is in_progress"),
            "the structural rule travels with the timing rule: {prompt}"
        );
        assert!(
            prompt.contains("Do not call it") && prompt.contains("inside a single step"),
            "one call per tool call would burn rounds for nothing: {prompt}"
        );
        assert!(
            prompt.contains("Never mark a step completed just to make the checklist look"),
            "a checklist that lies to look current is worse than a stale one: {prompt}"
        );
    }

    /// C2.3B §30 D/F — C2.3A's contract is untouched: the plan block may ask
    /// for a plan before *mutations*, but nothing in the prompt may frame
    /// navigation as optional or as a lesser action, and no guidance may force
    /// a tool choice.
    #[test]
    fn plan_guidance_does_not_demote_navigation() {
        let prompt = PromptBuilder::new().require_explicit_plan(true).build();
        assert!(
            prompt.contains("update_plan"),
            "multi-step mutation still expects a plan"
        );
        for demoting in ["optional read-only explore", "first substantive action"] {
            assert!(
                !prompt.contains(demoting),
                "C2.3A removed the navigation wall; the prompt must not rebuild it: {demoting:?}"
            );
        }
        assert!(
            prompt.contains("before you start changing files"),
            "the plan requirement must be scoped to mutation: {prompt}"
        );
    }

    /// C2.3B §15 — the dual of "re-read when a recollection may be stale":
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

    /// "Keep a short checklist" names the tool but sets no bar, and the observed
    /// failure is a plan whose steps merely restate the goal ("1. Build the CLI
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

    /// Strict status discipline. Without it the observed failure is
    /// batch-completing everything at the end (or jumping a step straight to
    /// completed), so the rendered plan lies about progress; and it keeps coding against a plan
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

    /// The Explicit planning gate used to ask for a prose plan — invisible to
    /// the UI and immediately stale. Multi-step plans must
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
                && prompt.contains("not a prose checklist")
                && prompt.contains("before you start changing files"),
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

    #[test]
    fn diagnosis_must_not_overclaim_or_lead_with_destruction() {
        let prompt = PromptBuilder::new()
            .turn_context(context(PermissionProfile::Assisted, false))
            .build();
        assert!(
            prompt.contains("sandbox reproduction"),
            "sandbox vs host must be named: {prompt}"
        );
        assert!(
            prompt.contains("pkill -f"),
            "destructive remediations must be called out: {prompt}"
        );
        assert!(
            prompt.contains("direct evidence"),
            "must require evidence before destructive steps"
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
            prompt.contains("escalate")
                && prompt.contains("unrestricted")
                && (prompt.contains("git pull") || prompt.contains("Git mutate")),
            "must route git mutate through the command's own FS escalation: {prompt}"
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
            !prompt.contains("escalate"),
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

    /// Choice forks must use structured options via request_user_input, not a
    /// prose "waiting for confirmation" pause (L1 decision-gate rule).
    #[test]
    fn base_prompt_requires_structured_decision_gates() {
        let prompt = PromptBuilder::new().build();
        assert!(
            prompt.contains("Decision gates") || prompt.contains("decision gate"),
            "base prompt must name decision gates: {prompt}"
        );
        assert!(
            prompt.contains("request_user_input"),
            "decision gates must route through request_user_input"
        );
        assert!(
            prompt.contains("options"),
            "decision gates must require concrete options"
        );
        assert!(
            prompt.contains("fake pause")
                || prompt.contains("waiting for confirmation")
                || prompt.contains("想问一下"),
            "must ban prose-only waiting as a substitute for a real gate"
        );
        assert!(
            prompt.contains("mutually exclusive")
                || prompt.contains("2–4")
                || prompt.contains("2-4"),
            "must specify option shape for choice forks"
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
        let planned = PromptBuilder::new()
            .require_explicit_plan(true)
            .build()
            .len();
        assert!(plain < 30_000, "system prompt grew to {plain} bytes");
        assert!(planned < 32_000, "with the plan block: {planned} bytes");
    }
}
