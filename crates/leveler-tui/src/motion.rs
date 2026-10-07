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
