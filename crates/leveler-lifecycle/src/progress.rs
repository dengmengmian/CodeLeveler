//! Cross-round progress ledger for the agent tool loop.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Coarse phase of the in-turn controller (UI / remote waiting surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    #[default]
    Active,
    AwaitingModel,
    ToolBatch,
    Closing,
    AwaitingUser,
    Terminal,
}

/// Policy caps for no-progress and post-closeout thrash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressCaps {
    pub no_progress_rounds: u32,
    pub closeout_deny_rounds: u32,
    pub continue_streak_cap: u32,
    /// Consecutive rounds with no *real* progress (no passing verification and no
    /// novel read/search) before the turn is force-stopped. Broader than
    /// `no_progress_rounds` (which only catches pure-observe thrash): this also
    /// catches the common "edit → run a check that keeps failing" spin. Set
    /// higher so legitimate fail→fix→pass iteration is not cut short.
    pub stagnation_rounds: u32,
    /// Consecutive rounds where EVERY attempted call was refused by a harness
    /// POLICY gate (plan gate / budgets / allowlist). These are the harness
    /// blocking itself, not agent stagnation (R006 R6-P1): at this cap a
    /// corrective directive is injected; at 2× the cap the turn stops with an
    /// honest policy-blocked reason instead of a fake "no progress".
    pub policy_blocked_rounds: u32,
}

impl Default for ProgressCaps {
    fn default() -> Self {
        Self {
            no_progress_rounds: 2,
            closeout_deny_rounds: 2,
            continue_streak_cap: 2,
            stagnation_rounds: 4,
            policy_blocked_rounds: 3,
        }
    }
}

/// Cross-round progress bookkeeping for one drive (and optionally continue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProgressLedger {
    pub round: u32,
    pub last_progress_round: u32,
    pub no_progress_streak: u32,
    /// Consecutive tool-using rounds with no *real* progress — no passing
    /// verification and no novel read/search. Edits alone do NOT reset it (an
    /// edit is not progress until a check passes), so a "keep editing while the
    /// check keeps failing" loop accumulates here and is force-stopped.
    #[serde(default)]
    pub stagnation_streak: u32,
    /// Consecutive all-policy-refused rounds (see `ProgressCaps::policy_blocked_rounds`).
    #[serde(default)]
    pub policy_blocked_streak: u32,
    pub closeout_deny_rounds: u32,
    pub closing: bool,
    pub phase: TurnPhase,
    pub objective_version: u32,
    /// Rounds spent across continues/resumes of this task epoch (absolute).
    #[serde(default)]
    pub cumulative_rounds: u32,
    /// Model tokens spent across continues/resumes of this task epoch.
    #[serde(default)]
    pub cumulative_model_tokens: u64,
    /// `run_command` / `shell_command` executions across the epoch.
    #[serde(default)]
    pub cumulative_commands: u32,
    /// Estimated model cost in micro-USD across the epoch.
    #[serde(default)]
    pub cumulative_cost_usd_micros: u64,
    /// Wall-clock ms spent driving this epoch (sum of drive durations).
    #[serde(default)]
    pub cumulative_duration_ms: u64,
    /// Distinct modified-file count across the epoch (upper bound for budgets).
    #[serde(default)]
    pub cumulative_modified_files: u32,
    /// Distinct modified paths for the epoch (source of truth for the count).
    /// Keeps continue/resume from double-counting re-edits of the same file.
    #[serde(default)]
    pub cumulative_modified_paths: Vec<String>,
    #[serde(default)]
    pub observe_hits: BTreeMap<String, (String, u32)>,
    /// MA-WA1: the one-shot keep-vs-delegate decision point was already offered
    /// this goal epoch. Persisted so continue/resume windows never re-ask
    /// (no nag loop across windows).
    #[serde(default)]
    pub delegation_decision_offered: bool,
    /// A `kept` disposition fact was already recorded this goal epoch, so
    /// continue/resume/repair windows do not re-record it (one fact per epoch,
    /// not one per window).
    #[serde(default)]
    pub delegation_kept_recorded: bool,
    /// A `delegated` disposition fact was already recorded this goal epoch.
    #[serde(default)]
    pub delegation_delegated_recorded: bool,
    /// The one event-driven delegation reconsideration was already raised this
    /// goal epoch (MA-WA1 repair), so later windows never raise it again.
    #[serde(default)]
    pub delegation_reconsidered: bool,
    /// Settled background-child results whose settlement notice the parent has
    /// not acted on yet (no successful non-observe call in a round at-or-after
    /// the notice becoming model-visible). Nonzero at a window boundary is
    /// material orchestration debt: the continuation layer may open a bounded
    /// follow-up window while the goal's total round budget still permits it,
    /// instead of terminalizing with a settled child result stranded
    /// (settlement × continuation seam — FA-2 / ORC-B1).
    #[serde(default)]
    pub unconsumed_child_settlements: u32,
    /// V2 background children still running when this snapshot was taken, as
    /// `id|nickname|role|scope` records. In-process children do not survive a
    /// restart: a resumed run reads this, tells the model truthfully which
    /// delegations were lost (scope released, re-delegate if still needed),
    /// and clears it. Normally drained to empty before a run returns.
    #[serde(default)]
    pub outstanding_children: Vec<String>,
    /// Total children ever accepted by `spawn_agent` in this task epoch —
    /// settled children included, refused spawns excluded. Durable so the
    /// total-delegation cap is a property of the task, not of one drive: a
    /// turn, window, or process boundary must not hand the model a fresh
    /// quota. Reviewer children are harness-owned and never consume it.
    #[serde(default)]
    pub children_spawned_total: u32,
    /// The user explicitly denied network elevation this task epoch.
    /// Host-side re-request guard; not a durable project rule.
    #[serde(default)]
    pub denied_network: bool,
    /// The user explicitly denied unrestricted-filesystem elevation this
    /// task epoch.
    #[serde(default)]
    pub denied_unrestricted_fs: bool,
}

impl ProgressLedger {
    pub fn with_objective_version(mut self, version: u32) -> Self {
        self.objective_version = version;
        self
    }

    /// True when this ledger must **not** be seeded into a fresh Content turn
    /// (task already closing/terminal — a new user message is a new epoch).
    pub fn is_terminal_for_inheritance(&self) -> bool {
        self.closing || matches!(self.phase, TurnPhase::Closing | TurnPhase::Terminal)
    }

    pub fn enter_closing(&mut self) {
        self.closing = true;
        self.phase = TurnPhase::Closing;
    }

    pub fn enter_terminal(&mut self) {
        self.closing = true;
        self.phase = TurnPhase::Terminal;
    }

    /// Fold one finished drive's spend into the epoch totals.
    pub fn accumulate_drive(&mut self, rounds: u32, model_tokens: u64) {
        self.cumulative_rounds = self.cumulative_rounds.saturating_add(rounds);
        self.cumulative_model_tokens = self.cumulative_model_tokens.saturating_add(model_tokens);
    }

    /// Absolute epoch spend snapshot (used when a drive ends or is mid-flight).
    pub fn set_epoch_spend(
        &mut self,
        rounds: u32,
        model_tokens: u64,
        commands: u32,
        cost_usd_micros: u64,
        duration_ms: u64,
        modified_files: u32,
    ) {
        self.cumulative_rounds = rounds;
        self.cumulative_model_tokens = model_tokens;
        self.cumulative_commands = commands;
        self.cumulative_cost_usd_micros = cost_usd_micros;
        self.cumulative_duration_ms = duration_ms;
        self.cumulative_modified_files = modified_files;
    }

    /// Merge paths into the epoch set and keep `cumulative_modified_files` in sync.
    pub fn merge_modified_paths<I, S>(&mut self, paths: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for path in paths {
            let path = path.as_ref();
            if path.is_empty() {
                continue;
            }
            if !self.cumulative_modified_paths.iter().any(|p| p == path) {
                self.cumulative_modified_paths.push(path.to_string());
            }
        }
        self.cumulative_modified_files = self.cumulative_modified_paths.len() as u32;
    }

    /// Fold another ledger's spend into this epoch (sub-agent → parent rollup).
    ///
    /// Does **not** add child wall-clock duration: parent duration is wall time
    /// of the parent drive (children may run concurrently). Commands / tokens /
    /// cost / files still roll up.
    pub fn absorb_child_spend(&mut self, child: &ProgressLedger) {
        self.cumulative_rounds = self
            .cumulative_rounds
            .saturating_add(child.cumulative_rounds);
        self.cumulative_model_tokens = self
            .cumulative_model_tokens
            .saturating_add(child.cumulative_model_tokens);
        self.cumulative_commands = self
            .cumulative_commands
            .saturating_add(child.cumulative_commands);
        self.cumulative_cost_usd_micros = self
            .cumulative_cost_usd_micros
            .saturating_add(child.cumulative_cost_usd_micros);
        self.merge_modified_paths(child.cumulative_modified_paths.iter().cloned());
    }

    /// Fresh epoch after /clear, /compact, or checkpoint restore — no inheritance.
    pub fn new_context_epoch() -> Self {
        let mut led = Self::default();
        led.enter_terminal();
        led
    }

    pub fn note_progress(&mut self, round: u32) {
        self.round = round;
        self.last_progress_round = round;
        self.no_progress_streak = 0;
    }

    pub fn note_no_progress_round(&mut self, round: u32) {
        self.round = round;
        self.no_progress_streak = self.no_progress_streak.saturating_add(1);
    }

    /// A round where every attempted call was refused by harness policy.
    pub fn note_policy_blocked_round(&mut self, round: u32) {
        self.round = round;
        self.policy_blocked_streak = self.policy_blocked_streak.saturating_add(1);
    }

    /// Any non-policy-blocked round clears the policy streak.
    pub fn clear_policy_blocked(&mut self) {
        self.policy_blocked_streak = 0;
    }

    /// At the cap: inject a corrective directive (do not stop yet).
    pub fn should_escalate_policy_blocked(&self, caps: ProgressCaps) -> bool {
        self.policy_blocked_streak == caps.policy_blocked_rounds
    }

    /// At 2x the cap the model has ignored both the per-call refusals AND the
    /// injected directive: stop honestly as policy-blocked.
    pub fn should_hard_stop_policy_blocked(&self, caps: ProgressCaps) -> bool {
        self.policy_blocked_streak >= caps.policy_blocked_rounds.saturating_mul(2)
    }

    pub fn note_closeout_deny_round(&mut self) {
        self.closeout_deny_rounds = self.closeout_deny_rounds.saturating_add(1);
        self.phase = TurnPhase::Closing;
    }

    pub fn fingerprint_content(content: &str) -> String {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in content.as_bytes() {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        format!("{:016x}:{}", hash, content.len())
    }

    pub fn should_refuse_observe_in_closing(&self) -> bool {
        self.closing
    }

    pub fn should_hard_stop_closeout(&self, caps: ProgressCaps) -> bool {
        self.closing && self.closeout_deny_rounds >= caps.closeout_deny_rounds
    }

    pub fn should_hard_stop_no_progress(&self, caps: ProgressCaps) -> bool {
        self.no_progress_streak >= caps.no_progress_rounds
    }

    /// Record a tool-using round for the unified stagnation guard. `made_progress`
    /// must be true ONLY for real forward motion: a verification-class command
    /// that passed, or a novel successful read/search. Edits and failing checks
    /// are NOT progress, so a loop that keeps editing while its check keeps
    /// failing accumulates here and is eventually force-stopped.
    pub fn note_round_outcome(&mut self, made_progress: bool) {
        if made_progress {
            self.stagnation_streak = 0;
        } else {
            self.stagnation_streak = self.stagnation_streak.saturating_add(1);
        }
    }

    pub fn should_hard_stop_stagnation(&self, caps: ProgressCaps) -> bool {
        self.stagnation_streak >= caps.stagnation_rounds
    }

    pub fn allows_engine_continue(&self, caps: ProgressCaps) -> bool {
        self.no_progress_streak < caps.continue_streak_cap
            && self.closeout_deny_rounds < caps.closeout_deny_rounds
            && !self.human_boundary_seen()
    }

    /// Record a human explicit denial. Capabilities accumulate for the epoch.
    pub fn record_human_denial(&mut self, network: bool, unrestricted_fs: bool) {
        if network {
            self.denied_network = true;
        }
        if unrestricted_fs {
            self.denied_unrestricted_fs = true;
        }
    }

    /// True when the user said no to at least one elevation this epoch.
    pub fn human_boundary_seen(&self) -> bool {
        self.denied_network || self.denied_unrestricted_fs
    }

    /// True when `request` overlaps a capability the user already denied
    /// (same or broader — `full_access` includes a denied `network`).
    pub fn covers_denied_request(&self, network: bool, unrestricted_fs: bool) -> bool {
        (network && self.denied_network) || (unrestricted_fs && self.denied_unrestricted_fs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_stable_and_sensitive() {
        let a = ProgressLedger::fingerprint_content("git status\nok");
        let b = ProgressLedger::fingerprint_content("git status\nok");
        let c = ProgressLedger::fingerprint_content("git status\nok\n");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn closeout_and_streak_caps() {
        let caps = ProgressCaps::default();
        let mut led = ProgressLedger::default();
        led.enter_closing();
        led.note_closeout_deny_round();
        assert!(!led.should_hard_stop_closeout(caps));
        led.note_closeout_deny_round();
        assert!(led.should_hard_stop_closeout(caps));

        let mut led2 = ProgressLedger::default();
        led2.note_no_progress_round(1);
        led2.note_no_progress_round(2);
        assert!(led2.should_hard_stop_no_progress(caps));
        assert!(!led2.allows_engine_continue(caps));
    }

    #[test]
    fn human_denial_covers_same_and_broader_but_not_other_axis() {
        let mut led = ProgressLedger::default();
        assert!(!led.human_boundary_seen());
        led.record_human_denial(true, false);
        assert!(led.human_boundary_seen());
        assert!(led.covers_denied_request(true, false));
        assert!(
            led.covers_denied_request(true, true),
            "full_access includes denied network"
        );
        assert!(
            !led.covers_denied_request(false, true),
            "unrelated filesystem request is still askable"
        );
        led.record_human_denial(false, true);
        assert!(led.covers_denied_request(false, true));
        assert!(led.covers_denied_request(true, true));
    }

    #[test]
    fn human_denial_blocks_engine_continue() {
        let caps = ProgressCaps::default();
        let mut led = ProgressLedger::default();
        assert!(led.allows_engine_continue(caps));
        led.record_human_denial(false, true);
        assert!(
            !led.allows_engine_continue(caps),
            "supervisor must not DriveGoalAgain past a human no"
        );
    }

    #[test]
    fn missing_denial_fields_default_on_old_snapshots() {
        let led: ProgressLedger = serde_json::from_str(
            r#"{"round":0,"last_progress_round":0,"no_progress_streak":0,"closeout_deny_rounds":0,"closing":false,"phase":"active","objective_version":0}"#,
        )
        .unwrap();
        assert!(!led.denied_network);
        assert!(!led.denied_unrestricted_fs);
    }

    #[test]
    fn stagnation_streak_stops_repeated_no_progress_but_a_pass_resets_it() {
        let caps = ProgressCaps::default(); // stagnation_rounds = 4
        let mut led = ProgressLedger::default();

        // Three no-progress rounds (edit + failing check): accumulates, no stop.
        for _ in 0..3 {
            led.note_round_outcome(false);
        }
        assert_eq!(led.stagnation_streak, 3);
        assert!(!led.should_hard_stop_stagnation(caps));

        // A round with real progress (check passed / novel read) resets it, so
        // legitimate fail→fail→fail→pass iteration is never cut short.
        led.note_round_outcome(true);
        assert_eq!(led.stagnation_streak, 0);
        assert!(!led.should_hard_stop_stagnation(caps));

        // Four consecutive no-progress rounds → force-stop.
        for _ in 0..4 {
            led.note_round_outcome(false);
        }
        assert!(led.should_hard_stop_stagnation(caps));
    }

    #[test]
    fn terminal_for_inheritance_and_accumulate() {
        let mut led = ProgressLedger::default();
        assert!(!led.is_terminal_for_inheritance());
        led.enter_closing();
        assert!(led.is_terminal_for_inheritance());
        led.accumulate_drive(5, 1200);
        assert_eq!(led.cumulative_rounds, 5);
        assert_eq!(led.cumulative_model_tokens, 1200);
        led.accumulate_drive(3, 800);
        assert_eq!(led.cumulative_rounds, 8);
        assert_eq!(led.cumulative_model_tokens, 2000);
        led.enter_terminal();
        assert!(led.is_terminal_for_inheritance());
        assert_eq!(led.phase, TurnPhase::Terminal);
    }

    #[test]
    fn epoch_spend_and_context_reset() {
        let mut led = ProgressLedger::default();
        led.set_epoch_spend(4, 900, 7, 12_000, 5_000, 3);
        assert_eq!(led.cumulative_commands, 7);
        assert_eq!(led.cumulative_cost_usd_micros, 12_000);
        assert_eq!(led.cumulative_duration_ms, 5_000);
        assert_eq!(led.cumulative_modified_files, 3);
        let fresh = ProgressLedger::new_context_epoch();
        assert!(fresh.is_terminal_for_inheritance());
        assert_eq!(fresh.cumulative_commands, 0);
    }

    #[test]
    fn merge_paths_is_distinct_and_absorb_rolls_up_child() {
        let mut parent = ProgressLedger::default();
        parent.merge_modified_paths(["a.rs", "b.rs"]);
        parent.merge_modified_paths(["a.rs"]); // re-edit: no double count
        assert_eq!(parent.cumulative_modified_files, 2);
        assert_eq!(parent.cumulative_modified_paths, vec!["a.rs", "b.rs"]);

        let mut child = ProgressLedger::default();
        child.set_epoch_spend(2, 100, 3, 50, 10, 0);
        child.merge_modified_paths(["b.rs", "c.rs"]);
        parent.absorb_child_spend(&child);
        assert_eq!(parent.cumulative_commands, 3);
        assert_eq!(parent.cumulative_model_tokens, 100);
        assert_eq!(parent.cumulative_cost_usd_micros, 50);
        // Wall duration is parent-only; child duration must not inflate it.
        assert_eq!(parent.cumulative_duration_ms, 0);
        assert_eq!(parent.cumulative_modified_files, 3);
        assert!(parent.cumulative_modified_paths.contains(&"c.rs".into()));
    }

    #[test]
    fn text_only_quiet_streak_blocks_engine_continue() {
        let caps = ProgressCaps::default();
        let mut led = ProgressLedger::default();
        // Goal quiet rounds (no tools, no update_goal) must feed the same streak.
        led.note_no_progress_round(1);
        assert!(led.allows_engine_continue(caps));
        led.note_no_progress_round(2);
        assert!(!led.allows_engine_continue(caps));
        assert!(led.should_hard_stop_no_progress(caps));
    }
}
