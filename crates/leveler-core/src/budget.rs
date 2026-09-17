//! Optional resource budgets shared by the runtime.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A coarse resource budget shared by the runtime. Concrete per-step budgets
/// (e.g. `StepBudget`) live in higher crates but reuse these fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceBudget {
    /// Maximum model tokens (input + output) allowed; `None` is unlimited.
    pub max_model_tokens: Option<u64>,
    /// Maximum auditable model cost in micro-USD; `None` is unlimited.
    pub max_cost_usd_micros: Option<u64>,
    /// Maximum wall-clock duration; `None` is unlimited.
    #[serde(default, with = "optional_duration_secs")]
    pub max_duration: Option<Duration>,
}

impl ResourceBudget {
    /// Fraction (0.0..=1.0+) of the token budget consumed so far.
    pub fn token_pressure(&self, spent_tokens: u64) -> f32 {
        self.max_model_tokens
            .map(|max| {
                if max == 0 {
                    1.0
                } else {
                    spent_tokens as f32 / max as f32
                }
            })
            .unwrap_or(0.0)
    }
}

mod optional_duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        d.map(|duration| duration.as_secs()).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?.map(Duration::from_secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_pressure_is_ratio() {
        let b = ResourceBudget {
            max_model_tokens: Some(100),
            ..Default::default()
        };
        assert!((b.token_pressure(70) - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn zero_budget_is_full_pressure() {
        let b = ResourceBudget {
            max_model_tokens: Some(0),
            ..Default::default()
        };
        assert_eq!(b.token_pressure(0), 1.0);
    }

    #[test]
    fn duration_serializes_as_seconds() {
        let b = ResourceBudget {
            max_duration: Some(Duration::from_secs(1800)),
            ..Default::default()
        };
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["max_duration"], 1800);
    }

    #[test]
    fn missing_optional_limits_deserialize_as_unlimited() {
        let budget: ResourceBudget = serde_json::from_str("{}").unwrap();
        assert_eq!(budget, ResourceBudget::default());
    }
}
