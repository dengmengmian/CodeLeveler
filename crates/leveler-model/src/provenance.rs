//! Which route produced a piece of model output.
//!
//! Reasoning is not portable between models. A provider's `reasoning_content`
//! channel has no signature, so a replay is only meaningful as *this model's
//! own* continued thought: handing one model's private reasoning to another
//! model would present foreign content as native context and spend tokens on a
//! voice the provider never produced.
//!
//! Pi (`packages/ai/src/api/transform-messages.ts`) draws the same boundary
//! with `provider === model.provider && api === model.api && model === model.id`.
//! CodeLeveler needs the same fact persisted, because a resumed session has no
//! other record of which model authored a historical assistant turn. The
//! route identity therefore rides on the assistant [`crate::Message`] that the
//! runtime assembled, not on the request that produced it.
//!
//! A provider alias is this configuration's identity for one deployment: each
//! `[providers.*]` entry has exactly one `base_url`. Two gateways serving the
//! same wire model id are therefore two routes (`deepseek/deepseek-flash` vs
//! `ark/deepseek-flash`), which is what makes "same model, different gateway"
//! detectable. Where the provider layer also knows the deployment (`base_url`),
//! it records it; a route identity that did not record one does not claim to
//! know it.

use serde::{Deserialize, Serialize};

use crate::profile::{ModelProfile, ProtocolKind};

/// The route that produced an assistant turn: provider, wire model, protocol,
/// and — when the provider layer knows it — the deployment behind that route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RouteIdentity {
    /// `[providers.*]` id. One provider entry addresses exactly one deployment.
    pub provider: String,
    /// The wire model id sent as `model`, not the user-facing alias.
    pub model: String,
    /// The protocol the route speaks. Absent on a payload that did not record
    /// it, which is not the same as "the same protocol".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ProtocolKind>,
    /// The endpoint the route addressed, when the provider layer knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment: Option<String>,
}

impl RouteIdentity {
    /// The identity a model profile describes. The protocol is a profile fact;
    /// the deployment is not, so it stays absent here and the provider layer
    /// may add it.
    pub fn of_profile(profile: &ModelProfile) -> Self {
        Self {
            provider: profile.provider.clone(),
            model: profile.model_id.clone(),
            api: Some(profile.protocol),
            deployment: None,
        }
    }

    /// Whether historical output from `self` may be replayed to `current`.
    ///
    /// Provider, wire model and protocol must agree. A recorded deployment must
    /// also agree; an unrecorded one is unknown, not a mismatch. When either
    /// side is `None` the caller is reasoning about "no provenance" and must
    /// use [`ReasoningProvenance::Unknown`](crate::ReasoningProvenance) rather
    /// than calling this.
    pub fn is_same_route(&self, current: &Self) -> bool {
        self.provider == current.provider
            && self.model == current.model
            && self.api == current.api
            && match (&self.deployment, &current.deployment) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(provider: &str, model: &str) -> RouteIdentity {
        RouteIdentity {
            provider: provider.into(),
            model: model.into(),
            api: Some(ProtocolKind::OpenAiChat),
            deployment: None,
        }
    }

    #[test]
    fn the_same_route_matches_itself() {
        assert!(
            route("deepseek", "deepseek-flash").is_same_route(&route("deepseek", "deepseek-flash"))
        );
    }

    #[test]
    fn another_gateway_is_another_route() {
        assert!(
            !route("deepseek", "deepseek-flash").is_same_route(&route("ark", "deepseek-flash"))
        );
    }

    #[test]
    fn another_model_of_the_same_provider_is_another_route() {
        assert!(
            !route("deepseek", "deepseek-flash")
                .is_same_route(&route("deepseek", "deepseek-v4-pro"))
        );
    }

    #[test]
    fn a_recorded_deployment_must_agree_but_an_absent_one_is_unknown() {
        let mut a = route("deepseek", "deepseek-flash");
        a.deployment = Some("https://api.deepseek.com".into());
        let mut b = route("deepseek", "deepseek-flash");
        b.deployment = Some("https://taotoken.net/api/v1".into());
        assert!(!a.is_same_route(&b));
        // The contract a policy resolves from a profile records no deployment;
        // that is unknown, not a conflict.
        assert!(a.is_same_route(&route("deepseek", "deepseek-flash")));
    }
}
