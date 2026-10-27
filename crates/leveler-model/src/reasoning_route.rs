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
//! [`ReasoningReplayEnforcement::Required`] rule (DeepSeek) rejects a request
//! that weakens it, so a weaker declaration is a [`ReasoningRouteConflict`]
//! instead of a provider 400. A [`ReasoningReplayEnforcement::Default`] rule
//! (GLM, Kimi) is the documented default for tool loops; a deployment that
//! knows it disabled preservation may opt out explicitly.

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
    /// Kimi thinking models: within a tool-call loop all reasoning content from
    /// the context must be sent back; preserved thinking keeps every historical
    /// assistant message's `reasoning_content` as-is.
    ///
    /// Source: <https://platform.kimi.ai/docs/guide/use-thinking-models>
    Kimi,
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
    } else if wire.starts_with("kimi") {
        Some(WireReasoningFamily::Kimi)
    } else {
        None
    }
}

/// The protocol requirement this route's model family imposes, if any.
///
/// Only the OpenAI-Chat wire has a `reasoning_content` assistant field. The
/// Messages protocol carries authenticated thinking blocks instead, and the
/// Responses/Gemini adapters do not exist in this build, so a family fact never
/// invents a channel on those protocols.
///
/// The model profile's `capabilities.reasoning` flag is deliberately NOT
/// consulted: it is a user-declared model fact, while this requirement belongs
/// to the provider's wire protocol. DeepSeek's thinking is a server-side
/// default, so a config that forgot to declare reasoning would otherwise be
/// exactly the config that sends a tool request the provider rejects.
pub fn required_reasoning_replay(
    protocol: ProtocolKind,
    model_id: &str,
) -> Option<RequiredReasoningReplay> {
    if protocol != ProtocolKind::OpenAiChat {
        return None;
    }
    let family = wire_family(model_id)?;
    Some(RequiredReasoningReplay {
        scope: ReasoningReplayScope::WhenToolsPresent,
        missing: match family {
            WireReasoningFamily::DeepSeek => MissingReasoningReplay::EmptyString,
            WireReasoningFamily::Glm | WireReasoningFamily::Kimi => MissingReasoningReplay::Omit,
        },
        enforcement: match family {
            WireReasoningFamily::DeepSeek => ReasoningReplayEnforcement::Required,
            WireReasoningFamily::Glm | WireReasoningFamily::Kimi => {
                ReasoningReplayEnforcement::Default
            }
        },
    })
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
        let Some(required) = required_reasoning_replay(profile.protocol, &profile.model_id) else {
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
        let Some(required) = required_reasoning_replay(profile.protocol, &profile.model_id) else {
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
