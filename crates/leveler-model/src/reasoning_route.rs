//! Which wire model families impose a historical-reasoning requirement.
//!
//! [`crate::CompatibilityConfig`] records what a route *declares*. A route can
//! still be talking to a model family whose protocol requires historical
//! reasoning to be replayed even when the user never wrote that fact down.
//! This module is the one place that knows those family requirements, and
//! [`crate::ReasoningReplayContract::for_route`] is the one resolution entry.
//!
//! # Why a table and not a protocol switch
//!
//! The requirement is a property of the *model family's* wire contract, not of
//! a CodeLeveler provider alias: the same DeepSeek model is served through
//! `api.deepseek.com`, a gateway such as Ark or Taotoken, and a local proxy,
//! under different `[providers.*]` ids. Configuration keyed on the provider
//! alias would miss every alternative endpoint; keying on the *wire* model id
//! (the value sent as `model`) is the route's real identity.
//!
//! A family entry only states what the provider's own documentation requires.
//! A model whose family is not listed keeps the conservative
//! [`crate::ReasoningReplayScope::Never`] default: CodeLeveler must not send a
//! field a route never asked for.
//!
//! # Precedence
//!
//! 1. Provider/model protocol capability (this table).
//! 2. The user's explicit declaration.
//! 3. The compatibility default (`never`, omit an empty key).
//!
//! A family entry declares how strongly its rule binds. A
//! [`ReasoningReplayEnforcement::Required`] rule (DeepSeek, Kimi K3) rejects a
//! request that weakens it, so a weaker declaration is a
//! [`ReasoningRouteConflict`] instead of a provider 400. A
//! [`ReasoningReplayEnforcement::Default`] rule (GLM) is the documented default
//! for tool loops; a deployment that knows it disabled preserved thinking may
//! opt out explicitly.
//!
//! The family is not the whole story. Kimi's older thinking models only require
//! the channel when the route actually reasons, so their rule reads the
//! profile's `capabilities.reasoning`; DeepSeek's does not, because its thinking
//! is a server-side default that a config can forget to declare.

use crate::profile::{ModelProfile, ProtocolKind};
use crate::projection::{
    MissingReasoningReplay, ReasoningIntegrity, ReasoningReplayContract, ReasoningReplayScope,
    ReasoningRepresentation,
};

/// How strongly a wire family's replay rule binds a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningReplayEnforcement {
    /// The provider rejects a tool-bearing request that omits the channel
    /// (DeepSeek: HTTP 400). An explicit weaker declaration is a startup error.
    Required,
    /// The family documents replay as the correct default for tool loops, but
    /// a deployment may turn preservation off (for example a standard GLM
    /// endpoint with preserved thinking disabled). Replay is the default; an
    /// explicit declaration may opt out.
    Default,
}

/// The replay a wire family requires when its protocol carries a tools array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequiredReasoningReplay {
    pub scope: ReasoningReplayScope,
    pub missing: MissingReasoningReplay,
    pub enforcement: ReasoningReplayEnforcement,
}

/// An explicit declaration that would make a required protocol request illegal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningRouteConflict {
    pub model_id: String,
    pub detail: String,
}

impl std::fmt::Display for ReasoningRouteConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "model `{}`: {}", self.model_id, self.detail)
    }
}

impl std::error::Error for ReasoningRouteConflict {}

/// The documented wire families with a historical-reasoning channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireReasoningFamily {
    /// DeepSeek thinking mode: a request carrying `tools` must replay the full
    /// `reasoning_content` of every historical assistant turn, including turns
    /// that did not call a tool. A turn that captured none carries the empty
    /// string (measured against the live API).
    ///
    /// Sources:
    /// * <https://api-docs.deepseek.com/guides/thinking_mode/> ("If the request
    ///   carries the `tools` parameter: the `reasoning_content` of all previous
    ///   turns should be passed back"; tool calls section: "must be fully
    ///   passed back ... even for turns where the model did not perform a tool
    ///   call").
    /// * <https://api-docs.deepseek.com/guides/tool_calls/>
    DeepSeek,
    /// GLM thinking models: interleaved thinking with tools requires the
    /// thinking blocks to be preserved and returned with the tool results;
    /// preserved thinking requires the complete, unmodified reasoning.
    ///
    /// Source: <https://docs.z.ai/guides/capabilities/thinking-mode>
    Glm,
    /// Kimi K3 and its point releases: always thinking, and the provider
    /// requires the complete assistant message back — `reasoning_content`
    /// included — so a tool-bearing request is malformed without it. The empty
    /// string covers a turn that captured none, exactly as DeepSeek needs.
    ///
    /// Sources:
    /// * <https://platform.kimi.ai/docs/guide/use-reasoning-effort> ("For
    ///   multi-turn conversations and tool calls, K3 requires the complete
    ///   assistant message returned by the API to be passed back ... including
    ///   `reasoning_content` and `tool_calls`")
    /// * <https://huggingface.co/moonshotai/Kimi-K3> (same requirement)
    KimiK3,
    /// Kimi's earlier thinking models (K2.5/K2.6/K2.7): while thinking is on,
    /// a tool-call turn must carry its `reasoning_content` back, but a turn that
    /// produced none carries no key — the protocol never asked for a
    /// placeholder. Preservation is a mode these models can also run without,
    /// so the requirement is read only for a route that declares it reasons.
    ///
    /// Sources:
    /// * <https://platform.kimi.ai/docs/guide/use-thinking-models>
    /// * measured 400s when the field is missing on a tool-call turn
    ///   (<https://github.com/BerriAI/litellm/issues/26156>)
    KimiToolLoop,
}

fn wire_family(model_id: &str) -> Option<WireReasoningFamily> {
    // The wire id is what the request sends as `model`. Gateways may prefix it
    // with their own namespace (`deepseek/deepseek-chat`,
    // `accounts/x/models/glm-5.3`), so the last segment is the model identity.
    let wire = model_id
        .rsplit('/')
        .next()
        .unwrap_or(model_id)
        .trim()
        .to_ascii_lowercase();
    if wire.starts_with("deepseek") {
        Some(WireReasoningFamily::DeepSeek)
    } else if wire.starts_with("glm") {
        Some(WireReasoningFamily::Glm)
    } else if is_kimi_k3(&wire) {
        Some(WireReasoningFamily::KimiK3)
    } else if wire.starts_with("kimi") || wire.starts_with("k2") || wire.starts_with("k3") {
        Some(WireReasoningFamily::KimiToolLoop)
    } else {
        None
    }
}

/// K3 and its point releases (`k3`, `k3-256k`, `k3.1-pro`, `kimi-k3`,
/// `kimi-k3.1-pro-highspeed`). Their request format differs from K2's: effort
/// is a top-level `reasoning_effort`, thinking cannot be turned off, and the
/// complete assistant message must come back.
fn is_kimi_k3(wire: &str) -> bool {
    let id = wire.strip_prefix("kimi-").unwrap_or(wire);
    id == "k3" || id.starts_with("k3-") || id.starts_with("k3.")
}

/// The protocol requirement this route's model family imposes, if any.
///
/// Only the OpenAI-Chat wire has a `reasoning_content` assistant field. The
/// Messages protocol carries authenticated thinking blocks instead, and the
/// Responses/Gemini adapters do not exist in this build, so a family fact never
/// invents a channel on those protocols.
///
/// This entry point answers the family's unconditional form. The model
/// profile's `capabilities.reasoning` flag is deliberately NOT consulted here:
/// DeepSeek's thinking is a server-side default, so a config that forgot to
/// declare reasoning would otherwise be exactly the config that sends a tool
/// request the provider rejects. Kimi's older thinking models are the case that
/// does depend on the declaration; they resolve through
/// [`required_reasoning_replay_for`], which every runtime path uses.
pub fn required_reasoning_replay(
    protocol: ProtocolKind,
    model_id: &str,
) -> Option<RequiredReasoningReplay> {
    required_replay_for_route(protocol, model_id, true)
}

/// The family requirement for a route, given whether the route declares that it
/// reasons. `declares_reasoning` is only consulted by families whose
/// requirement is conditioned on thinking being on; DeepSeek's server-side
/// default deliberately ignores it, and K3 always thinks.
fn required_replay_for_route(
    protocol: ProtocolKind,
    model_id: &str,
    declares_reasoning: bool,
) -> Option<RequiredReasoningReplay> {
    if protocol != ProtocolKind::OpenAiChat {
        return None;
    }
    let family = wire_family(model_id)?;
    let (missing, enforcement) = match family {
        WireReasoningFamily::DeepSeek => (
            MissingReasoningReplay::EmptyString,
            ReasoningReplayEnforcement::Required,
        ),
        WireReasoningFamily::Glm => (
            MissingReasoningReplay::Omit,
            ReasoningReplayEnforcement::Default,
        ),
        WireReasoningFamily::KimiK3 => (
            MissingReasoningReplay::EmptyString,
            ReasoningReplayEnforcement::Required,
        ),
        WireReasoningFamily::KimiToolLoop => {
            if !declares_reasoning {
                // A non-thinking Kimi model has no historical reasoning channel
                // to protect, so it gets no rule of its own.
                return None;
            }
            (
                MissingReasoningReplay::Omit,
                ReasoningReplayEnforcement::Required,
            )
        }
    };
    Some(RequiredReasoningReplay {
        scope: ReasoningReplayScope::WhenToolsPresent,
        missing,
        enforcement,
    })
}

/// The family requirement for one configured route.
pub fn required_reasoning_replay_for(profile: &ModelProfile) -> Option<RequiredReasoningReplay> {
    required_replay_for_route(
        profile.protocol,
        &profile.model_id,
        profile.capabilities.reasoning,
    )
}

fn replay_rank(scope: ReasoningReplayScope) -> u8 {
    match scope {
        ReasoningReplayScope::Never => 0,
        ReasoningReplayScope::WhenToolsPresent => 1,
        ReasoningReplayScope::Always => 2,
    }
}

impl ReasoningReplayContract {
    /// Resolve the contract for a configured route.
    ///
    /// `leveler login`, a hand-written config, the lab, dogfood and every
    /// client resolve through this function, so the capability precedence has
    /// exactly one implementation and no entry point depends on another having
    /// written the compatibility fields.
    ///
    /// An explicit declaration may strengthen a documented default, and it may
    /// opt out of a [`ReasoningReplayEnforcement::Default`] family rule. It may
    /// not weaken a [`ReasoningReplayEnforcement::Required`] rule. Callers that
    /// need a diagnostic call [`Self::check_route`] first; this function
    /// resolves to the legal contract so an unchecked caller cannot construct
    /// an illegal request.
    pub fn for_route(profile: &ModelProfile) -> Self {
        let declared = Self::resolve(profile.protocol, &profile.compatibility);
        let Some(required) = required_reasoning_replay_for(profile) else {
            return declared;
        };

        let scope = match profile.compatibility.reasoning_replay_scope {
            Some(ReasoningReplayScope::Always) => ReasoningReplayScope::Always,
            // A documented default (not a hard requirement) may be opted out.
            Some(ReasoningReplayScope::Never)
                if required.enforcement == ReasoningReplayEnforcement::Default =>
            {
                ReasoningReplayScope::Never
            }
            _ => required.scope,
        };
        if scope == ReasoningReplayScope::Never {
            return declared;
        }
        let missing = if required.missing == MissingReasoningReplay::EmptyString
            || profile.compatibility.reasoning_content_key_required == Some(true)
        {
            MissingReasoningReplay::EmptyString
        } else {
            MissingReasoningReplay::Omit
        };
        Self {
            representation: ReasoningRepresentation::RawAssistantField,
            scope,
            missing,
            // The family documents that the retained reasoning must reach the
            // provider verbatim, so a retention arm may not strip or rewrite a
            // turn while it stays in the active history.
            integrity: ReasoningIntegrity::RetainedExact,
            state: declared.state,
            native_context: declared.native_context,
        }
    }

    /// Report an explicit declaration that contradicts a required protocol.
    ///
    /// The message names the model, the field and the provider outcome so a
    /// configuration mistake is diagnosable without a live 400.
    pub fn check_route(profile: &ModelProfile) -> Result<(), ReasoningRouteConflict> {
        let Some(required) = required_reasoning_replay_for(profile) else {
            return Ok(());
        };

        if required.enforcement == ReasoningReplayEnforcement::Required
            && let Some(declared) = profile.compatibility.reasoning_replay_scope
            && replay_rank(declared) < replay_rank(required.scope)
        {
            return Err(ReasoningRouteConflict {
                model_id: profile.model_id.clone(),
                detail: format!(
                    "this wire protocol requires historical reasoning replay on every \
                     tool-bearing request, but `reasoning_replay_scope = {}` disables it; \
                     the provider rejects such a request with HTTP 400 — remove the \
                     override or declare `{}`/`always`",
                    declared.as_str(),
                    required.scope.as_str()
                ),
            });
        }

        if required.enforcement == ReasoningReplayEnforcement::Required
            && required.missing == MissingReasoningReplay::EmptyString
            && profile.compatibility.reasoning_content_key_required == Some(false)
        {
            return Err(ReasoningRouteConflict {
                model_id: profile.model_id.clone(),
                detail: "this wire protocol requires the `reasoning_content` key on replayed \
                         assistant turns that captured no reasoning, but \
                         `reasoning_content_key_required = false` omits it; the provider rejects \
                         such a request with HTTP 400 — remove the override or set it to true"
                    .into(),
            });
        }

        Ok(())
    }
}
