//! C5-S3: the context budget as a controlled runtime state.
//!
//! `ContextPolicy` (engine resolver) is policy — resolved once at legitimate
//! lifecycle points. This module is the STATE the policy governs while a task
//! runs, plus the one pure decision function that moves it. The split is
//! deliberate: policy never mutates mid-task, state never leaks back into the
//! resolver, and the compatibility mirror `policy.context_budget` stays the
//! initial value forever.
//!
//! Core rule (cache-aware trigger): expansion keeps the request prefix,
//! folding rewrites it — so when the estimate crosses the current threshold,
//! an evidence-backed expansion is preferred over a compaction. Evidence is
//! authoritative runtime state only: repeated-read pressure measured by
//! `RepeatedReadGuard` after a fold, or a repair turn escalation passed down
//! from the engine. Budget pressure alone is when the decision happens, never
//! why the budget grows.

/// Why the budget expanded — recorded on the event so every climb is
/// attributable to a real signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpansionReason {
    /// Re-reads of unchanged ranges after a fold: the fold dropped something
    /// the model still needs (measured by `RepeatedReadGuard::total_trips`).
    RereadPressure,
    /// The engine entered a verification-repair turn: failure evidence may
    /// reference folded state.
    RepairEscalation,
}

impl ExpansionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RereadPressure => "reread_pressure",
            Self::RepairEscalation => "repair_escalation",
        }
    }
}

/// The mutable per-task context budget. Lives in the drive loop; durably
/// reconstructed from `ContextExpanded` events on resume, never from memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBudgetState {
    pub current_budget: u32,
    pub expansion_count: u32,
    pub compaction_count: u32,
    /// Guard trip total at the moment of the last fold — the baseline the
    /// re-read-pressure delta is measured against.
    pub trips_at_last_compaction: u32,
    /// Trip total consumed by the last expansion, so the identical signal
    /// cannot be spent twice without new evidence.
    pub trips_spent: u32,
    /// Repair escalation is single-use per grant.
    pub repair_evidence_available: bool,
    pub crossed_reliable_context: bool,
    pub last_expansion_reason: Option<ExpansionReason>,
}

impl ContextBudgetState {
    pub fn new(initial_budget: u32, repair_evidence: bool) -> Self {
        Self {
            current_budget: initial_budget,
            expansion_count: 0,
            compaction_count: 0,
            trips_at_last_compaction: 0,
            trips_spent: 0,
            repair_evidence_available: repair_evidence,
            crossed_reliable_context: false,
            last_expansion_reason: None,
        }
    }
}

/// What the drive loop should do about context this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextAction {
    Keep,
    Expand {
        from: u32,
        to: u32,
        reason: ExpansionReason,
        crossed_reliable: bool,
    },
    Compact,
}

/// How many new guard trips after a fold count as re-read pressure. From the
/// C5 spec's expansion signal: ≥3 re-reads of folded-away content.
pub const REREAD_PRESSURE_TRIPS: u32 = 3;

/// The one decision point. Pure and deterministic: same inputs, same action —
/// which is also what makes the state replayable from the event log.
///
/// `tiers` is the resolver's ladder (sorted, deduped, clamped);
/// `reliable_context` marks the quality boundary that plain evidence may not
/// cross — only a repair escalation (strong evidence) climbs past it, and the
/// crossing is recorded, not hidden.
pub fn decide_context_action(
    adaptive: bool,
    tiers: &[u32],
    reliable_context: u32,
    state: &ContextBudgetState,
    estimated_tokens: u64,
    guard_trips_now: u32,
) -> ContextAction {
    if state.current_budget == 0 || estimated_tokens <= u64::from(state.current_budget) {
        return ContextAction::Keep;
    }
    if !adaptive {
        return ContextAction::Compact;
    }
    let Some(&next) = tiers.iter().find(|&&t| t > state.current_budget) else {
        // Already at the top of the ladder: folding is all that is left.
        return ContextAction::Compact;
    };
    let crossing = next > reliable_context;

    // Re-read pressure: new trips since the last fold, beyond what an earlier
    // expansion already consumed. Meaningless before any fold has happened —
    // nothing has been dropped yet for the model to miss.
    let trips_delta = guard_trips_now
        .saturating_sub(state.trips_at_last_compaction)
        .saturating_sub(state.trips_spent);
    let reread_pressure = state.compaction_count > 0 && trips_delta >= REREAD_PRESSURE_TRIPS;

    // Strong evidence (repair escalation) may take any step, including across
    // the quality boundary. Normal evidence stops at reliable_context.
    if state.repair_evidence_available {
        return ContextAction::Expand {
            from: state.current_budget,
            to: next,
            reason: ExpansionReason::RepairEscalation,
            crossed_reliable: crossing,
        };
    }
    if reread_pressure && !crossing {
        return ContextAction::Expand {
            from: state.current_budget,
            to: next,
            reason: ExpansionReason::RereadPressure,
            crossed_reliable: false,
        };
    }
    ContextAction::Compact
}

/// Apply an expansion to the state (the drive loop calls this after emitting
/// the event, so replay and live execution share one transition).
pub fn apply_expansion(
    state: &mut ContextBudgetState,
    to: u32,
    reason: ExpansionReason,
    crossed_reliable: bool,
    guard_trips_now: u32,
) {
    state.current_budget = to;
    state.expansion_count += 1;
    state.last_expansion_reason = Some(reason);
    if crossed_reliable {
        state.crossed_reliable_context = true;
    }
    match reason {
        ExpansionReason::RereadPressure => {
            state.trips_spent = guard_trips_now.saturating_sub(state.trips_at_last_compaction);
        }
        ExpansionReason::RepairEscalation => state.repair_evidence_available = false,
    }
}

/// Record a fold: resets the re-read-pressure baseline to "now".
pub fn apply_compaction(state: &mut ContextBudgetState, guard_trips_now: u32) {
    state.compaction_count += 1;
    state.trips_at_last_compaction = guard_trips_now;
    state.trips_spent = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIERS: &[u32] = &[262_144, 524_288, 786_432];
    const RELIABLE: u32 = 786_432;

    fn state(budget: u32) -> ContextBudgetState {
        ContextBudgetState::new(budget, false)
    }

    #[test]
    fn under_budget_keeps() {
        let s = state(262_144);
        let a = decide_context_action(true, TIERS, RELIABLE, &s, 100_000, 0);
        assert_eq!(a, ContextAction::Keep);
    }

    #[test]
    fn disabled_reproduces_static_behavior() {
        // Ablation off = S2: over budget always folds, never expands.
        let s = state(262_144);
        let a = decide_context_action(false, TIERS, RELIABLE, &s, 300_000, 99);
        assert_eq!(a, ContextAction::Compact);
    }

    #[test]
    fn budget_pressure_alone_is_not_evidence() {
        // Adaptive, over budget, but no fold has happened and no repair —
        // pressure is when the decision happens, not why the budget grows.
        let s = state(262_144);
        let a = decide_context_action(true, TIERS, RELIABLE, &s, 300_000, 0);
        assert_eq!(a, ContextAction::Compact);
    }

    #[test]
    fn reread_pressure_after_a_fold_expands_one_tier() {
        let mut s = state(262_144);
        apply_compaction(&mut s, 1);
        let a = decide_context_action(true, TIERS, RELIABLE, &s, 300_000, 4);
        assert_eq!(
            a,
            ContextAction::Expand {
                from: 262_144,
                to: 524_288,
                reason: ExpansionReason::RereadPressure,
                crossed_reliable: false,
            },
            "3 new trips after a fold is the spec's re-read signal"
        );
    }

    #[test]
    fn reread_pressure_before_any_fold_is_meaningless() {
        // Trips without a preceding fold cannot mean "the fold dropped it".
        let s = state(262_144);
        let a = decide_context_action(true, TIERS, RELIABLE, &s, 300_000, 50);
        assert_eq!(a, ContextAction::Compact);
    }

    #[test]
    fn the_same_signal_cannot_be_spent_twice() {
        let mut s = state(262_144);
        apply_compaction(&mut s, 0);
        let a = decide_context_action(true, TIERS, RELIABLE, &s, 300_000, 3);
        let ContextAction::Expand { to, reason, .. } = a else {
            panic!("first expansion must fire");
        };
        apply_expansion(&mut s, to, reason, false, 3);
        // Same trip total, higher pressure: no NEW evidence → fold.
        let again = decide_context_action(true, TIERS, RELIABLE, &s, 600_000, 3);
        assert_eq!(again, ContextAction::Compact);
        // Three genuinely new trips re-arm the signal.
        let rearmed = decide_context_action(true, TIERS, RELIABLE, &s, 600_000, 6);
        assert!(matches!(rearmed, ContextAction::Expand { to: 786_432, .. }));
    }

    #[test]
    fn normal_evidence_stops_at_the_quality_boundary() {
        let tiers = &[262_144, 524_288, 786_432, 900_000];
        let mut s = state(786_432);
        apply_compaction(&mut s, 0);
        let a = decide_context_action(true, tiers, RELIABLE, &s, 800_000, 10);
        assert_eq!(
            a,
            ContextAction::Compact,
            "re-read pressure alone must not cross reliable_context"
        );
    }

    #[test]
    fn repair_escalation_may_cross_the_boundary_and_is_single_use() {
        let tiers = &[262_144, 524_288, 786_432, 900_000];
        let mut s = ContextBudgetState::new(786_432, true);
        let a = decide_context_action(true, tiers, RELIABLE, &s, 800_000, 0);
        let ContextAction::Expand {
            to,
            reason,
            crossed_reliable,
            ..
        } = a
        else {
            panic!("repair escalation must expand");
        };
        assert_eq!(to, 900_000);
        assert_eq!(reason, ExpansionReason::RepairEscalation);
        assert!(crossed_reliable, "the crossing is recorded, not hidden");
        apply_expansion(&mut s, to, reason, crossed_reliable, 0);
        assert!(s.crossed_reliable_context);
        // The grant is spent: the next pressure folds.
        let again = decide_context_action(true, tiers, RELIABLE, &s, 950_000, 0);
        assert_eq!(again, ContextAction::Compact);
    }

    #[test]
    fn at_the_ladder_top_only_folding_remains() {
        let mut s = state(786_432);
        apply_compaction(&mut s, 0);
        let a = decide_context_action(true, TIERS, RELIABLE, &s, 900_000, 10);
        assert_eq!(a, ContextAction::Compact);
    }

    #[test]
    fn expansion_is_one_tier_per_transition() {
        let mut s = state(262_144);
        apply_compaction(&mut s, 0);
        let a = decide_context_action(true, TIERS, RELIABLE, &s, 700_000, 5);
        assert!(
            matches!(a, ContextAction::Expand { to: 524_288, .. }),
            "even a large overshoot climbs exactly one tier: {a:?}"
        );
    }

    #[test]
    fn zero_budget_means_unlimited_and_never_acts() {
        let s = state(0);
        let a = decide_context_action(true, TIERS, RELIABLE, &s, 10_000_000, 50);
        assert_eq!(a, ContextAction::Keep);
    }

    #[test]
    fn replay_is_deterministic() {
        // Same inputs, same transitions → identical state. This is what lets
        // resume rebuild the budget from the event log.
        let run = || {
            let mut s = state(262_144);
            apply_compaction(&mut s, 2);
            if let ContextAction::Expand {
                to,
                reason,
                crossed_reliable,
                ..
            } = decide_context_action(true, TIERS, RELIABLE, &s, 300_000, 5)
            {
                apply_expansion(&mut s, to, reason, crossed_reliable, 5);
            }
            s
        };
        assert_eq!(run(), run());
    }
}
