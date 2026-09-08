//! Cross-round progress ledger for the agent tool loop.

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

/// Resource caps for the mechanical no-progress watchdog. Two things tick it,
/// both structural: a round in which every attempted call was refused before
/// it ran, and a goal-mode drive that *terminated* without `update_goal` (one
/// tick per stalled drive, so `continue_active_goal` cannot re-drive it
/// forever).
///
/// This is not a quiet-round rule. A running drive never ticks: any executed
/// tool, failed or not, counts as progress, and rounds that merely produce
/// text are untouched. Ending a run because recent rounds looked quiet was
/// measured and refuted — seven successful runs went quiet for 26 to 129
/// rounds and still finished. No cap here reads the model's work for meaning;
/// the absolute round ceiling and budgets remain the hard boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressCaps {
    /// Consecutive no-progress ticks (all-refused round, or a stalled goal
    /// drive) before the turn stops. Not a calibrated threshold: both inputs
    /// are binary, so this only answers "how many repeats of a fully blocked
    /// round to allow".
    pub no_progress_rounds: u32,
}

impl Default for ProgressCaps {
    fn default() -> Self {
        Self {
            no_progress_rounds: 2,
        }
    }
}

/// Cross-round progress bookkeeping for one drive (and optionally continue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProgressLedger {
    pub round: u32,
    pub last_progress_round: u32,
    pub no_progress_streak: u32,
    pub closing: bool,
    pub phase: TurnPhase,
    pub objective_version: u32,
    /// Rounds spent across continues/resumes of this task epoch (absolute).
    #[serde(default)]
    pub cumulative_rounds: u32,
    /// Model tokens spent across continues/resumes of this task epoch.
    ///
    /// This is the number a token budget admits on. It is the durable
    /// `model_requests` total for this session PLUS
    /// [`Self::cumulative_estimated_model_tokens`] — subtract that and what
    /// remains must equal the ledger exactly.
    #[serde(default)]
    pub cumulative_model_tokens: u64,
    /// The share of [`Self::cumulative_model_tokens`] that no provider
    /// reported.
    ///
    /// A gateway that returns zero usage must not silently switch the token
    /// budget off, so the loop bills a transcript estimate instead. That
    /// estimate is an admission input, not an accounting fact, and it has no
    /// durable row behind it. Carried separately so the two can never be
    /// confused for one another: the ledger stays reconcilable, and a caller
    /// that needs the audited figure subtracts this.
    #[serde(default)]
    pub cumulative_estimated_model_tokens: u64,
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

    /// Fold one finished drive's rounds into the epoch totals.
    ///
    /// Rounds only. Spend has exactly one way in — [`Self::absorb_request_spend`],
    /// fed by the finalized records — and a second writer for tokens is how the
    /// runtime came to hold a number the bill could not account for.
    pub fn accumulate_drive_rounds(&mut self, rounds: u32) {
        self.cumulative_rounds = self.cumulative_rounds.saturating_add(rounds);
    }

    /// Absolute epoch spend snapshot (used when a drive ends or is mid-flight).
    ///
    /// `estimated_model_tokens` is the unreported share of `model_tokens` —
    /// see [`Self::cumulative_estimated_model_tokens`].
    #[allow(clippy::too_many_arguments)]
    pub fn set_epoch_spend(
        &mut self,
        rounds: u32,
        model_tokens: u64,
        estimated_model_tokens: u64,
        commands: u32,
        cost_usd_micros: u64,
        duration_ms: u64,
        modified_files: u32,
    ) {
        self.cumulative_rounds = rounds;
        self.cumulative_model_tokens = model_tokens;
        self.cumulative_estimated_model_tokens = estimated_model_tokens;
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

    /// Fold another ledger's non-spend work into this epoch (sub-agent →
    /// parent rollup): rounds, commands and touched files.
    ///
    /// Does **not** add child wall-clock duration: parent duration is wall time
    /// of the parent drive (children may run concurrently).
    ///
    /// Deliberately does **not** add the child's tokens or cost either. A
    /// child's model calls reach the parent as records — the same records that
    /// become its `model_requests` rows — and are folded there. Adding the
    /// child's own summed totals here as well would bill every delegated token
    /// twice, and the two paths do not even agree: the child's ledger counts
    /// its rounds, while its rows also carry the folds and advisory calls it
    /// made. Spend has one path in, and it is the record.
    pub fn absorb_child_work(&mut self, child: &ProgressLedger) {
        self.cumulative_rounds = self
            .cumulative_rounds
            .saturating_add(child.cumulative_rounds);
        self.cumulative_commands = self
            .cumulative_commands
            .saturating_add(child.cumulative_commands);
        self.merge_modified_paths(child.cumulative_modified_paths.iter().cloned());
    }

    /// Fold one model call's spend into the epoch.
    ///
    /// The single entry point for tokens and cost: whoever made the call — the
    /// root loop, a delegated child, the closure reviewer, a bounded advisory
    /// — its spend arrives here, once, from the same finalized record that
    /// becomes its durable row.
    pub fn absorb_request_spend(
        &mut self,
        reported_tokens: u64,
        estimated_tokens: u64,
        cost_usd_micros: u64,
    ) {
        self.cumulative_model_tokens = self
            .cumulative_model_tokens
            .saturating_add(reported_tokens)
            .saturating_add(estimated_tokens);
        self.cumulative_estimated_model_tokens = self
            .cumulative_estimated_model_tokens
            .saturating_add(estimated_tokens);
        self.cumulative_cost_usd_micros = self
            .cumulative_cost_usd_micros
            .saturating_add(cost_usd_micros);
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

    pub fn should_hard_stop_no_progress(&self, caps: ProgressCaps) -> bool {
        self.no_progress_streak >= caps.no_progress_rounds
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
    fn no_progress_streak_caps() {
        let caps = ProgressCaps::default();
        let mut led2 = ProgressLedger::default();
        led2.note_no_progress_round(1);
        led2.note_no_progress_round(2);
        assert!(led2.should_hard_stop_no_progress(caps));
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
    fn missing_denial_fields_default_on_old_snapshots() {
        let led: ProgressLedger = serde_json::from_str(
            r#"{"round":0,"last_progress_round":0,"no_progress_streak":0,"closeout_deny_rounds":0,"stagnation_streak":3,"closing":false,"phase":"active","objective_version":0}"#,
        )
        .unwrap();
        assert!(!led.denied_network);
        assert!(!led.denied_unrestricted_fs);
    }

    /// Snapshots written by older runtimes carry the streak counters of the
    /// deleted thrash/stagnation/policy watchdogs. They are ignored on read
    /// and never feed a decision again.
    #[test]
    fn legacy_watchdog_counters_are_ignored_on_old_snapshots() {
        let led: ProgressLedger = serde_json::from_str(
            r#"{"round":9,"last_progress_round":1,"no_progress_streak":0,"closeout_deny_rounds":5,"stagnation_streak":9,"policy_blocked_streak":9,"observe_hits":{"k":["v",3]},"closing":true,"phase":"closing","objective_version":0}"#,
        )
        .unwrap();
        assert_eq!(led.round, 9);
        assert!(led.closing);
        assert_eq!(led.no_progress_streak, 0);
    }

    #[test]
    fn terminal_for_inheritance_and_accumulate() {
        let mut led = ProgressLedger::default();
        assert!(!led.is_terminal_for_inheritance());
        led.enter_closing();
        assert!(led.is_terminal_for_inheritance());
        led.accumulate_drive_rounds(5);
        assert_eq!(led.cumulative_rounds, 5);
        led.accumulate_drive_rounds(3);
        assert_eq!(led.cumulative_rounds, 8);
        led.enter_terminal();
        assert!(led.is_terminal_for_inheritance());
        assert_eq!(led.phase, TurnPhase::Terminal);
    }

    #[test]
    fn epoch_spend_and_context_reset() {
        let mut led = ProgressLedger::default();
        led.set_epoch_spend(4, 900, 0, 7, 12_000, 5_000, 3);
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
        child.set_epoch_spend(2, 100, 0, 3, 50, 10, 0);
        child.merge_modified_paths(["b.rs", "c.rs"]);
        parent.absorb_child_work(&child);
        assert_eq!(parent.cumulative_commands, 3);
        assert_eq!(parent.cumulative_rounds, 2);
        // Spend does NOT ride the child's ledger: its records carry it, and
        // folding both would bill every delegated token twice.
        assert_eq!(parent.cumulative_model_tokens, 0);
        assert_eq!(parent.cumulative_cost_usd_micros, 0);
        // Wall duration is parent-only; child duration must not inflate it.
        assert_eq!(parent.cumulative_duration_ms, 0);
        assert_eq!(parent.cumulative_modified_files, 3);
        assert!(parent.cumulative_modified_paths.contains(&"c.rs".into()));
    }

    /// Spend arrives one record at a time, and the unreported share stays
    /// separable from the share a durable row can vouch for.
    #[test]
    fn request_spend_keeps_the_estimated_share_separable() {
        let mut led = ProgressLedger::default();
        led.absorb_request_spend(1_000, 0, 40);
        led.absorb_request_spend(0, 250, 0);
        assert_eq!(led.cumulative_model_tokens, 1_250, "admission sees both");
        assert_eq!(led.cumulative_estimated_model_tokens, 250);
        assert_eq!(
            led.cumulative_model_tokens - led.cumulative_estimated_model_tokens,
            1_000,
            "what remains is exactly what the ledger can vouch for"
        );
        assert_eq!(led.cumulative_cost_usd_micros, 40);
    }

    #[test]
    fn text_only_quiet_rounds_feed_the_hard_stop() {
        let caps = ProgressCaps::default();
        let mut led = ProgressLedger::default();
        // Goal quiet rounds (no tools, no update_goal) feed the same streak
        // as all-refused rounds: both are mechanical facts about the round.
        led.note_no_progress_round(1);
        assert!(!led.should_hard_stop_no_progress(caps));
        led.note_no_progress_round(2);
        assert!(led.should_hard_stop_no_progress(caps));
    }
}
