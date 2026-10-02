//! The Thinking Level a user chooses, and how it projects onto what a model can
//! actually do.
//!
//! Two vocabularies, deliberately:
//!
//! - [`ThinkingLevel`] is **CodeLeveler's**: `auto`, `off`, `minimal`, `low`,
//!   `medium`, `high`, `max`. It is what a user reads, writes in config, and
//!   sees in the status line, and it means the same thing on every model.
//! - [`ReasoningEffort`] is the **route's**: the string a provider's own API
//!   spells (`minimal` … `xhigh`). It is a wire detail and never reaches a user,
//!   a config file, or a status line.
//!
//! The projection is capability-driven, never name-driven: a level asks for a
//! *relative* amount of thinking, and the model's declared [`ReasoningConfig`]
//! says which levels exist and what they are called on that route. `Max` is
//! "this model's highest", not a fixed value — `xhigh` where that exists and
//! `high` where it does not, with the user's intent (`max`) preserved either
//! way, so a model upgrade re-resolves instead of pinning an old setting.
//!
//! What each level asks for:
//!
//! - `auto` — no preference: CodeLeveler's declared default for this model
//!   stands, and a model that declares none gets no reasoning field at all, so
//!   the provider's own default applies.
//! - `off` — ask the route to turn thinking off, where the route can be asked.
//! - `minimal` … `high` — the same-named native level, and only where the model
//!   declares it.
//! - `max` — the strongest native level the model declares.

use serde::{Deserialize, Serialize};

use crate::profile::{
    ReasoningConfig, ReasoningEffort, ReasoningStyle, normalize_reasoning_effort,
};

/// How hard the user wants the model to think, in CodeLeveler's own words.
///
/// The order is the product contract: weakest → strongest, with `Auto` first
/// because "no preference" is the default and the recommended answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// No preference. CodeLeveler's default for the model applies; a model that
    /// declares none gets no reasoning field at all.
    Auto,
    /// Ask the route to disable extra thinking, where it can be asked.
    Off,
    Minimal,
    Low,
    Medium,
    High,
    /// The strongest level this model declares — not a fixed setting.
    Max,
}

impl ThinkingLevel {
    /// Every level, weakest → strongest, as the product offers them.
    pub const ALL: [Self; 7] = [
        Self::Auto,
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Max,
    ];

    /// The spelling used in config, on the wire, and on `/thinking`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }

    /// Parse the public spelling. Case- and whitespace-insensitive, because a
    /// person is typing this.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// The comma-separated public vocabulary, for a message a user can paste
    /// straight back into a config file or a command.
    pub fn values() -> String {
        Self::ALL
            .iter()
            .map(|level| level.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The native level this name asks for, when the name *is* a native level.
    ///
    /// `Auto` asks for nothing of its own, `Off` is not an effort at all, and
    /// `Max` depends on what the model declares — [`ThinkingCapabilities`]
    /// answers those three.
    fn native(self) -> Option<ReasoningEffort> {
        match self {
            Self::Minimal => Some(ReasoningEffort::Minimal),
            Self::Low => Some(ReasoningEffort::Low),
            Self::Medium => Some(ReasoningEffort::Medium),
            Self::High => Some(ReasoningEffort::High),
            Self::Auto | Self::Off | Self::Max => None,
        }
    }
}

impl std::fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a model's thinking can be controlled at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingAccess {
    /// The model does not reason.
    Unsupported,
    /// The model reasons, but the caller cannot ask for a different amount:
    /// there is no knob, or the route has no way to name one.
    Fixed,
    /// The caller can choose a level.
    Adjustable,
}

/// What one level resolves to on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingProjection {
    /// Send no reasoning field: the route's own default applies.
    Omit,
    /// Send this native level.
    Effort(ReasoningEffort),
    /// Ask the route to disable thinking (`thinking: {"type": "disabled"}` on
    /// the styles that speak it).
    Disabled,
}

/// Which levels a model can distinguish, and what each one projects to.
///
/// Built from the model's declared capability, so the answer is a fact about the
/// route rather than a guess about a model name. A route with two levels offers
/// two; a route that cannot be told to stop thinking does not offer `off`; and a
/// route with nothing above `high` still offers `max`, because `max` means "the
/// strongest this model has" rather than a level of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingCapabilities {
    access: ThinkingAccess,
    /// The levels this model accepts *and* a selector may offer, weakest first.
    levels: Vec<ThinkingLevel>,
    /// Whether the route can be told to disable thinking explicitly.
    supports_disable: bool,
    /// The route's own levels, weakest first, deduplicated.
    supported: Vec<ReasoningEffort>,
    /// CodeLeveler's declared default for this model, if any.
    default: Option<ReasoningEffort>,
}

impl ThinkingCapabilities {
    /// Read a model's capability.
    ///
    /// `reasoning` is `ModelCapabilities::reasoning` — whether the model reasons
    /// at all. `config` is what the route declared about how to ask.
    pub fn of(reasoning: bool, config: &ReasoningConfig) -> Self {
        let adjustable = reasoning
            && config.style != ReasoningStyle::None
            && (!config.supported_efforts.is_empty() || config.default_effort.is_some());
        if !adjustable {
            // Fixed: it reasons, but nobody can ask for a different amount.
            // Unsupported: it does not reason at all. Two different sentences
            // for the user, so the difference is kept.
            return Self {
                access: if reasoning {
                    ThinkingAccess::Fixed
                } else {
                    ThinkingAccess::Unsupported
                },
                levels: Vec::new(),
                supports_disable: false,
                supported: Vec::new(),
                default: None,
            };
        }
        // The style is the route's own declaration of how it spells a reasoning
        // request, and only one of them has a word for "disabled".
        let supports_disable = matches!(config.style, ReasoningStyle::ThinkingFlag);
        let mut supported = if config.supported_efforts.is_empty() {
            config.default_effort.into_iter().collect::<Vec<_>>()
        } else {
            config.supported_efforts.clone()
        };
        supported.sort_by_key(|effort| reasoning_rank(*effort));
        supported.dedup();

        let mut levels = vec![ThinkingLevel::Auto];
        if supports_disable {
            levels.push(ThinkingLevel::Off);
        }
        for effort in &supported {
            let level = canonical_for(*effort);
            if !levels.contains(&level) {
                levels.push(level);
            }
        }
        // `max` is always an option: it names the strongest level this model
        // has, which is a real answer even when that level is called `high`.
        if !levels.contains(&ThinkingLevel::Max) {
            levels.push(ThinkingLevel::Max);
        }
        Self {
            access: ThinkingAccess::Adjustable,
            levels,
            supports_disable,
            supported,
            default: config.default_effort,
        }
    }

    pub fn access(&self) -> ThinkingAccess {
        self.access
    }

    pub fn is_adjustable(&self) -> bool {
        self.access == ThinkingAccess::Adjustable
    }

    /// The levels worth offering on this model, `auto` first.
    pub fn levels(&self) -> &[ThinkingLevel] {
        &self.levels
    }

    /// Whether this model can express this level — the question a command
    /// handler asks before accepting one, so an impossible request is answered
    /// with the real options instead of being silently rounded away.
    pub fn accepts(&self, level: ThinkingLevel) -> bool {
        self.levels.contains(&level)
    }

    /// What this level is on the wire, or `None` when this model cannot express
    /// it. Strict on purpose; a caller that must not lose a request uses
    /// [`ThinkingCapabilities::resolve`].
    pub fn project(&self, level: ThinkingLevel) -> Option<ThinkingProjection> {
        if !self.is_adjustable() {
            return None;
        }
        match level {
            // "No preference": CodeLeveler's declared default, or nothing at
            // all when the model declares none. Never an invented middle.
            ThinkingLevel::Auto => Some(self.project_default()),
            ThinkingLevel::Off if self.supports_disable => Some(ThinkingProjection::Disabled),
            ThinkingLevel::Off => None,
            // The strongest this model declares.
            ThinkingLevel::Max => self
                .supported
                .last()
                .copied()
                .map(ThinkingProjection::Effort),
            other => other
                .native()
                .map(|effort| ThinkingProjection::Effort(self.nearest(effort))),
        }
    }

    /// Like [`ThinkingCapabilities::project`], but never drops a request: a
    /// level this model cannot express becomes the nearest one it can. This is
    /// what keeps a session setting working across a model switch — an
    /// unavailable `off` becomes the model's lowest level instead of a
    /// parameter the provider would reject.
    pub fn resolve(&self, level: ThinkingLevel) -> Option<ThinkingProjection> {
        self.project(level).or_else(|| {
            if !self.is_adjustable() {
                return None;
            }
            match level {
                ThinkingLevel::Auto => None,
                ThinkingLevel::Off => self
                    .supported
                    .first()
                    .copied()
                    .map(ThinkingProjection::Effort),
                other => {
                    let effort = other.native().or_else(|| self.supported.last().copied())?;
                    Some(ThinkingProjection::Effort(self.nearest(effort)))
                }
            }
        })
    }

    /// What a bare `auto` sends.
    fn project_default(&self) -> ThinkingProjection {
        match self.default {
            Some(effort) => ThinkingProjection::Effort(effort),
            None => ThinkingProjection::Omit,
        }
    }

    fn nearest(&self, effort: ReasoningEffort) -> ReasoningEffort {
        normalize_reasoning_effort(effort, &self.supported).unwrap_or(effort)
    }
}

/// The canonical name for a native level.
///
/// The two vocabularies share `minimal`…`high`. Anything stronger is `max`,
/// because that is what the user asked for — the strongest this model has — and
/// `xhigh` is a provider's word, not CodeLeveler's.
fn canonical_for(effort: ReasoningEffort) -> ThinkingLevel {
    match effort {
        ReasoningEffort::Minimal => ThinkingLevel::Minimal,
        ReasoningEffort::Low => ThinkingLevel::Low,
        ReasoningEffort::Medium => ThinkingLevel::Medium,
        ReasoningEffort::High => ThinkingLevel::High,
        ReasoningEffort::XHigh | ReasoningEffort::Max => ThinkingLevel::Max,
    }
}

fn reasoning_rank(effort: ReasoningEffort) -> u8 {
    ReasoningEffort::ALL
        .iter()
        .position(|candidate| *candidate == effort)
        .unwrap_or(0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(style: ReasoningStyle, supported: &[ReasoningEffort]) -> ReasoningConfig {
        ReasoningConfig {
            style,
            supported_efforts: supported.to_vec(),
            default_effort: supported.last().copied(),
        }
    }

    fn adjustable(style: ReasoningStyle, supported: &[ReasoningEffort]) -> ThinkingCapabilities {
        ThinkingCapabilities::of(true, &config(style, supported))
    }

    fn effort(projection: ThinkingProjection) -> ReasoningEffort {
        match projection {
            ThinkingProjection::Effort(effort) => effort,
            other => panic!("expected an effort, got {other:?}"),
        }
    }

    /// The public vocabulary is exactly the seven words, in product order.
    #[test]
    fn the_public_vocabulary_is_seven_levels() {
        assert_eq!(
            ThinkingLevel::values(),
            "auto, off, minimal, low, medium, high, max"
        );
        for level in ThinkingLevel::ALL {
            assert_eq!(ThinkingLevel::parse(level.as_str()), Some(level));
            assert_eq!(level.to_string(), level.as_str());
        }
        // The provider's own words are not user words.
        for native in ["xhigh", "none", "on", "true"] {
            assert_eq!(ThinkingLevel::parse(native), None, "{native}");
        }
        assert_eq!(ThinkingLevel::parse("  MAX "), Some(ThinkingLevel::Max));
    }

    /// `high` and `max` are different requests: on a route with `xhigh`, `max`
    /// is the stronger one, and `high` stays `high`.
    #[test]
    fn high_and_max_are_distinct_when_the_route_has_more_than_high() {
        let caps = adjustable(
            ReasoningStyle::OpenAiEffort,
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ],
        );
        assert_eq!(
            effort(caps.project(ThinkingLevel::High).unwrap()),
            ReasoningEffort::High
        );
        assert_eq!(
            effort(caps.project(ThinkingLevel::Max).unwrap()),
            ReasoningEffort::XHigh
        );
        // The selector shows the model's real levels, not seven.
        assert_eq!(
            caps.levels(),
            &[
                ThinkingLevel::Auto,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::Max,
            ]
        );
        assert!(!caps.accepts(ThinkingLevel::Minimal));
        assert!(!caps.accepts(ThinkingLevel::Off));
    }

    /// A route that stops at `high`: `max` still means its strongest level, and
    /// the intent stays `max` even though the wire says `high`.
    #[test]
    fn max_is_the_strongest_this_model_has_even_when_that_is_high() {
        let caps = adjustable(
            ReasoningStyle::OpenAiEffort,
            &[
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
        );
        assert_eq!(
            effort(caps.project(ThinkingLevel::Max).unwrap()),
            ReasoningEffort::High
        );
        assert_eq!(
            effort(caps.project(ThinkingLevel::High).unwrap()),
            ReasoningEffort::High
        );
        assert!(caps.accepts(ThinkingLevel::Max));
    }

    /// A route with a single knob: `auto`, `off` (it can say disabled) and
    /// `max`, exactly as §34 of the product spec describes.
    #[test]
    fn a_two_state_route_offers_three_levels() {
        let caps = adjustable(ReasoningStyle::ThinkingFlag, &[ReasoningEffort::High]);
        assert_eq!(
            caps.levels(),
            &[
                ThinkingLevel::Auto,
                ThinkingLevel::Off,
                ThinkingLevel::High,
                ThinkingLevel::Max,
            ]
        );
        assert_eq!(
            caps.project(ThinkingLevel::Off),
            Some(ThinkingProjection::Disabled)
        );
        assert_eq!(
            effort(caps.project(ThinkingLevel::Max).unwrap()),
            ReasoningEffort::High
        );
    }

    /// `off` is offered only where the route can be told to stop.
    #[test]
    fn a_route_that_cannot_say_off_does_not_offer_it() {
        let caps = adjustable(
            ReasoningStyle::OpenAiEffort,
            &[ReasoningEffort::Low, ReasoningEffort::High],
        );
        assert!(!caps.accepts(ThinkingLevel::Off));
        assert_eq!(caps.project(ThinkingLevel::Off), None);
        // ...but a session that already says `off` must not send something the
        // provider would reject: it resolves to the lowest level instead.
        assert_eq!(
            effort(caps.resolve(ThinkingLevel::Off).unwrap()),
            ReasoningEffort::Low
        );
    }

    /// `auto` is not a level in disguise: it sends the model's declared default,
    /// and omits the field entirely when the model declares none.
    #[test]
    fn auto_sends_the_declared_default_or_nothing() {
        let caps = adjustable(
            ReasoningStyle::OpenAiEffort,
            &[ReasoningEffort::Low, ReasoningEffort::High],
        );
        assert_eq!(
            effort(caps.project(ThinkingLevel::Auto).unwrap()),
            ReasoningEffort::High,
            "the declared default is the strongest supported here"
        );
        let no_default = ThinkingCapabilities::of(
            true,
            &ReasoningConfig {
                style: ReasoningStyle::OpenAiEffort,
                supported_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
                default_effort: None,
            },
        );
        assert_eq!(
            no_default.project(ThinkingLevel::Auto),
            Some(ThinkingProjection::Omit)
        );
    }

    /// A level the route does not declare lands on its nearest declared one
    /// rather than being dropped.
    #[test]
    fn an_undeclared_level_rounds_to_the_nearest_declared_one() {
        let caps = adjustable(
            ReasoningStyle::OpenAiEffort,
            &[ReasoningEffort::Low, ReasoningEffort::High],
        );
        assert_eq!(
            effort(caps.resolve(ThinkingLevel::Medium).unwrap()),
            ReasoningEffort::High
        );
        assert_eq!(
            effort(caps.resolve(ThinkingLevel::Minimal).unwrap()),
            ReasoningEffort::Low
        );
    }

    /// Two sentences, two situations: a model that does not reason at all, and a
    /// model that reasons but cannot be asked for a different amount.
    #[test]
    fn unsupported_and_fixed_are_different_states() {
        let off = ThinkingCapabilities::of(false, &ReasoningConfig::default());
        assert_eq!(off.access(), ThinkingAccess::Unsupported);
        assert!(!off.is_adjustable());
        assert!(off.levels().is_empty());
        assert_eq!(off.project(ThinkingLevel::Max), None);

        let fixed = ThinkingCapabilities::of(
            true,
            &ReasoningConfig {
                style: ReasoningStyle::None,
                supported_efforts: Vec::new(),
                default_effort: None,
            },
        );
        assert_eq!(fixed.access(), ThinkingAccess::Fixed);
        assert!(!fixed.is_adjustable());
        assert!(fixed.levels().is_empty());
        assert_eq!(fixed.project(ThinkingLevel::Auto), None);
    }

    /// A route that declares a style but no levels at all is fixed, not
    /// adjustable: there is nothing to choose between.
    #[test]
    fn a_style_without_levels_is_not_adjustable() {
        let caps = ThinkingCapabilities::of(
            true,
            &ReasoningConfig {
                style: ReasoningStyle::AdaptiveThinking,
                supported_efforts: Vec::new(),
                default_effort: None,
            },
        );
        assert_eq!(caps.access(), ThinkingAccess::Fixed);
    }

    /// A route whose style carries a fixed budget declares no levels, so there
    /// is nothing to choose: it is fixed. Inventing a mapping from `max` to a
    /// larger budget would be guessing at a protocol the profile did not
    /// declare.
    #[test]
    fn a_budget_only_route_is_fixed() {
        let caps = adjustable(
            ReasoningStyle::BudgetedThinking {
                budget_tokens: 8192,
            },
            &[],
        );
        assert_eq!(caps.access(), ThinkingAccess::Fixed);
        assert!(!caps.is_adjustable());
        assert!(caps.levels().is_empty());
        assert_eq!(caps.project(ThinkingLevel::Max), None);
        assert_eq!(caps.resolve(ThinkingLevel::Max), None);
    }

    /// The canonical layer serializes as the public words, so a session record
    /// and a config file both hold `max` rather than `xhigh`.
    #[test]
    fn the_canonical_layer_round_trips_as_public_words() {
        let json = serde_json::to_string(&ThinkingLevel::Max).unwrap();
        assert_eq!(json, "\"max\"");
        assert_eq!(
            serde_json::from_str::<ThinkingLevel>("\"off\"").unwrap(),
            ThinkingLevel::Off
        );
        let all: Vec<ThinkingLevel> = ThinkingLevel::ALL.to_vec();
        let json = serde_json::to_string(&all).unwrap();
        assert_eq!(
            json,
            "[\"auto\",\"off\",\"minimal\",\"low\",\"medium\",\"high\",\"max\"]"
        );
    }
}
