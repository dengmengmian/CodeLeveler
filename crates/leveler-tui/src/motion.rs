//! Motion policy: whether the UI paints live animations at all.
//!
//! One switch, one owner. A user who asks for reduced motion (or runs the TUI
//! in a context where animation is noise) turns every animated surface off in
//! one place, and each such surface degrades to a static form that carries the
//! same state — never to a missing fact.
//!
//! Resolution order is env-only: `LEVELER_TUI_ANIMATION` (like
//! `LEVELER_TUI_RECORD` / `LEVELER_TUI_PROFILE`) — `0`, `false`, `off` or `no`
//! disables motion; anything else, and the absence of the variable, leaves it
//! on. There is no per-screen override: motion is a whole-UI preference.

/// Environment variable that turns live animations off.
pub const ENV_ANIMATION: &str = "LEVELER_TUI_ANIMATION";

/// One breath of the plan indicator's running glyph.
///
/// The busy tick repaints at 150 ms, so a 1.2 s period lands on eight stops —
/// a steady rise and fall that never reads as a flicker.
pub const PLAN_BREATH_PERIOD: std::time::Duration = std::time::Duration::from_millis(1200);

/// Where in its breath an indicator is at `clock`: `1.0` at the start of each
/// period, `0.0` at the half period, cosine in between.
///
/// Reads ELAPSED TIME, never a frame counter: a burst of stream deltas repaints
/// many frames a second, and a frame-counted phase would sweep proportionally
/// faster than its period (same rule as every other live animation).
pub fn breath_level(clock: std::time::Duration, period: std::time::Duration) -> f32 {
    if period.is_zero() {
        return 1.0;
    }
    let phase = (clock.as_secs_f64() % period.as_secs_f64()) / period.as_secs_f64();
    (0.5 + 0.5 * (phase * std::f64::consts::TAU).cos()) as f32
}

/// Whether live animations are enabled, from the process environment.
pub fn live_animation_enabled() -> bool {
    match leveler_core::environment().var(ENV_ANIMATION) {
        Some(raw) => parse_live_animation(&raw),
        None => true,
    }
}

/// Parse the switch. Only an explicit negative disables motion, so a typo
/// cannot silently remove a live indicator.
pub fn parse_live_animation(raw: &str) -> bool {
    !matches!(
        raw.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The breath is a full rise and fall per period, sampled from elapsed
    /// time: bright at the start, dim at the half period, bright again — and
    /// always inside [0, 1].
    #[test]
    fn the_breath_rises_and_falls_once_per_period() {
        use std::time::Duration;
        let period = PLAN_BREATH_PERIOD;
        let at = |ms| breath_level(Duration::from_millis(ms), period);
        assert!((at(0) - 1.0).abs() < 1e-6);
        assert!((at(600) - 0.0).abs() < 1e-6, "half period is the dim end");
        assert!((at(1200) - 1.0).abs() < 1e-6, "the next period starts over");
        assert!(at(300) < at(0) && at(300) > at(600), "monotone descent");
        for ms in (0..2400).step_by(37) {
            let level = at(ms);
            assert!((0.0..=1.0).contains(&level), "{ms}ms: {level}");
        }
        assert_eq!(breath_level(period, Duration::ZERO), 1.0);
    }

    #[test]
    fn only_an_explicit_negative_disables_motion() {
        for off in ["0", "false", "FALSE", " off ", "no"] {
            assert!(!parse_live_animation(off), "{off:?} must disable motion");
        }
        for on in ["1", "true", "yes", "on", "", "maybe"] {
            assert!(parse_live_animation(on), "{on:?} leaves motion on");
        }
    }
}
