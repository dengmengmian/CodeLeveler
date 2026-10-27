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
    /// Provider, wire model, protocol and — when either side recorded one —
    /// the deployment must agree, and an unrecorded deployment is NOT treated
    /// as agreement: "nobody wrote it down" cannot prove that two aliases
    /// address the same endpoint. Callers that need to tell "another route"
    /// from "unproven" ask [`Self::conflicts_with`].
    pub fn is_same_route(&self, current: &Self) -> bool {
        self.provider == current.provider
            && self.model == current.model
            && self.api == current.api
            && self.deployment == current.deployment
    }

    /// Whether this turn demonstrably came from a DIFFERENT route.
    ///
    /// The complement of [`Self::is_same_route`] is not "foreign": a route
    /// whose `api` or `deployment` one side did not record is unproven, not
    /// contradictory. Only a difference on a fact both sides (or the wire
    /// model itself) state is a conflict.
    pub fn conflicts_with(&self, current: &Self) -> bool {
        self.provider != current.provider
            || self.model != current.model
            || (self.api.is_some() && current.api.is_some() && self.api != current.api)
            || matches!(
                (&self.deployment, &current.deployment),
                (Some(a), Some(b)) if a != b
            )
    }
}

/// The deployment identity recorded on a route: scheme, host, port and path.
///
/// One endpoint has one identity. Two spellings of the same `base_url`
/// (trailing slashes, host letter case, an explicit default port) must not look
/// like two deployments — that would drop a model's own reasoning chain for no
/// protocol reason — and credentials that a deployment URL carries must never
/// reach a session record. The query, fragment and userinfo are therefore
/// dropped rather than preserved as an opaque string.
///
/// The scheme and path keep their own case (a path may be case-sensitive);
/// only the host is folded.
pub fn deployment_identity(base_url: &str) -> String {
    let without_fragment = base_url.trim().split('#').next().unwrap_or_default();
    let without_query = without_fragment.split('?').next().unwrap_or_default();
    let (scheme, rest) = match without_query.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => (String::new(), without_query),
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    // `user:secret@host` — the credentials are not part of the deployment's
    // identity, and must not be persisted with the turn that addressed it.
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    let authority = normalize_authority(authority, &scheme);
    let path = path.trim_end_matches('/');
    if scheme.is_empty() {
        format!("{authority}{path}")
    } else {
        format!("{scheme}://{authority}{path}")
    }
}

/// Lower-case the host and drop a port that only restates the scheme default.
fn normalize_authority(authority: &str, scheme: &str) -> String {
    // IPv6 literals are bracketed, so a `:` inside them is not a port separator.
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((host, tail)) => (
                format!("[{host}]"),
                tail.strip_prefix(':').filter(|port| !port.is_empty()),
            ),
            None => (authority.to_string(), None),
        },
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), Some(port).filter(|port| !port.is_empty())),
            None => (authority.to_string(), None),
        },
    };
    let host = host.to_ascii_lowercase();
    let default_port = match scheme {
        "http" => Some("80"),
        "https" => Some("443"),
        _ => None,
    };
    match port {
        Some(port) if Some(port) != default_port => format!("{host}:{port}"),
        _ => host,
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
    fn a_recorded_deployment_must_agree_and_an_absent_one_is_not_equality() {
        let mut a = route("deepseek", "deepseek-flash");
        a.deployment = Some(deployment_identity("https://api.deepseek.com"));
        let mut b = route("deepseek", "deepseek-flash");
        b.deployment = Some(deployment_identity("https://taotoken.net/api/v1"));
        assert!(!a.is_same_route(&b));
        assert!(a.conflicts_with(&b));

        // One endpoint recorded, the other not: that is unproven, never
        // sameness. Replaying on it would present reasoning captured from an
        // endpoint nobody can name as this endpoint's own.
        let unnamed = route("deepseek", "deepseek-flash");
        assert!(!a.is_same_route(&unnamed));
        assert!(!a.conflicts_with(&unnamed));
        assert!(!unnamed.is_same_route(&a));
        assert!(!unnamed.conflicts_with(&a));
    }

    #[test]
    fn one_endpoint_has_exactly_one_identity() {
        assert_eq!(
            deployment_identity("https://API.DeepSeek.com/v1/"),
            deployment_identity("https://api.deepseek.com/v1")
        );
        assert_eq!(
            deployment_identity("https://api.deepseek.com:443/v1"),
            deployment_identity("https://api.deepseek.com/v1")
        );
        assert_eq!(
            deployment_identity("  http://127.0.0.1:8791/api/v1//  "),
            "http://127.0.0.1:8791/api/v1"
        );
        // Different deployments stay different: the identity never collapses
        // two endpoints into one route just because they serve one model id.
        assert_ne!(
            deployment_identity("https://api.deepseek.com/v1"),
            deployment_identity("https://ark.cn-beijing.volces.com/api/coding/v3")
        );
        assert_ne!(
            deployment_identity("http://127.0.0.1:8791/api/v1"),
            deployment_identity("http://127.0.0.1:8792/api/v1")
        );
    }

    #[test]
    fn an_endpoint_identity_never_records_credentials() {
        let identity = deployment_identity(
            "https://user:secret@gw.example.com/v1?api_key=abc&tenant=1#fragment",
        );
        assert_eq!(identity, "https://gw.example.com/v1");
        for secret in ["user", "secret", "api_key", "abc", "fragment"] {
            assert!(
                !identity.contains(secret),
                "{identity} still carries {secret}"
            );
        }
    }
}
