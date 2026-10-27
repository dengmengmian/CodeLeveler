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
//! [`ReasoningReplayEnforcement::Required`] rule (DeepSeek, Kimi K2.7 and
//! later, Kimi K3/K3.1) rejects a request that weakens it, so a weaker
//! declaration is a [`ReasoningRouteConflict`] instead of a provider 400. A
//! [`ReasoningReplayEnforcement::Default`] rule (GLM without preserved
//! thinking) is the documented default for tool loops; a deployment that knows
//! it disabled preserved thinking may opt out explicitly.
//!
//! The family is not the whole story. DeepSeek's rule reads only the request
//! shape (`tools` present or not): the provider states the requirement that way
//! and its thinking is a server-side default a config can forget to declare.
//! GLM's rule also reads the mode the route declared, because the vendor
//! documents replay as the condition of preserved thinking: once the route asks
//! the endpoint to keep prior turns' thinking, every request owes that thinking
//! back, not just a tool-bearing one. Kimi K2.7 and later and K3/K3.1 have no
//! such switch at all — thinking and preservation are unconditional there — so
//! their rule is hard on every request.
//!
//! A route that asks the endpoint to PRESERVE prior thinking
//! (`preserved_thinking = true`) makes its family's rule binding: the
//! documented preserved mode requires the retained reasoning back, so
//! "preserve it, then send nothing" is a contradiction rather than a
//! deployment difference. The field that spells that request is per family
//! ([`PreservedThinkingWire`]) — GLM says `clear_thinking`, and a family with
//! no such field must not receive another endpoint's spelling.

use crate::profile::{ModelProfile, ProtocolKind, ReasoningStyle};
use crate::projection::{
    MissingReasoningReplay, ReasoningIntegrity, ReasoningReplayContract, ReasoningReplayScope,
    ReasoningRepresentation,
};

/// How strongly a wire family's replay rule binds a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningReplayEnforcement {
    /// The provider rejects a request that omits a required channel
    /// (DeepSeek: HTTP 400 on a tool-bearing request; Kimi: malformed request).
    /// An explicit weaker declaration is a startup error.
    Required,
    /// The family documents replay as the correct default for tool loops, but
    /// a deployment may turn preservation off (for example a standard GLM
    /// endpoint with preserved thinking disabled). Replay is the default; an
    /// explicit declaration may opt out.
    Default,
}

/// How a wire family spells "keep this route's previous thinking".
///
/// The fact is the same for every family that has the mode; the field is not.
/// A route that declared preservation must be asked for it in its own
/// endpoint's word, and a family with no such word must not receive another
/// endpoint's field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreservedThinkingWire {
    /// The family has no preserved-thinking request field: either the
    /// endpoint keeps nothing between requests (DeepSeek) or preservation is
    /// unconditional and cannot be asked for (Kimi K2.7+/K3). GLM's spelling is
    /// not one of theirs, so declaring the mode is not a request they can
    /// honour.
    None,
    /// GLM: `thinking: {type: "enabled", clear_thinking: false}` keeps prior
    /// turns' reasoning, and `clear_thinking: true` discards it. The endpoint's
    /// default depends on the plan (Coding Plan preserves, the standard API
    /// clears), so an unrequested mode sends neither field.
    ClearThinking,
}

/// The replay a wire family requires, and how strongly it binds.
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
    /// included — on multi-turn conversations and tool calls. The empty string
    /// covers a turn that captured none, exactly as DeepSeek needs. K3 takes no
    /// `thinking` field: effort is a top-level `reasoning_effort`, and its
    /// preservation is unconditional.
    ///
    /// Sources:
    /// * <https://platform.kimi.ai/docs/guide/use-thinking-models> (`thinking`:
    ///   no value for K3/K3.1; complete assistant message required)
    /// * <https://platform.kimi.ai/docs/api/models-overview> (`thinking` is a
    ///   K2.x-only request parameter)
    /// * <https://platform.kimi.ai/docs/guide/kimi-k3-1-pro-quickstart> ("Return
    ///   the complete assistant message unchanged in multi-turn conversations
    ///   and tool calls")
    KimiK3,
    /// Kimi K2.7 Code, its high-speed variant, K2.8 Preview and the Kimi Code
    /// ids that currently resolve to them: thinking is always on and Preserved
    /// Thinking is always on, so every historical assistant message's
    /// `reasoning_content` must come back — in an ordinary multi-turn
    /// conversation as much as in a tool loop. The endpoint accepts no other
    /// mode, and it asks for this one by omission: passing `thinking` is
    /// unnecessary and, for everything but
    /// `{type: "enabled", keep: "all"}`, an error. CodeLeveler sends no
    /// `thinking` field for this family.
    ///
    /// Sources:
    /// * <https://platform.kimi.ai/docs/guide/use-thinking-models> ("you do not
    ///   need to (and should not) pass the `thinking` parameter"; "Preserved
    ///   Thinking is always on and cannot be turned off")
    /// * <https://platform.kimi.ai/docs/api/models-overview> (`thinking` may be
    ///   omitted; if set, only `{type: "enabled", keep: "all"}` is accepted)
    /// * <https://www.kimi.com/code/docs/kimi-code/models> (Kimi Code model ids
    ///   and the K2.7 Code Highspeed / K2.8 Preview mapping)
    KimiK27,
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
    } else if is_kimi_k27(&wire) {
        Some(WireReasoningFamily::KimiK27)
    } else {
        None
    }
}

/// K3 and its point releases (`k3`, `k3-256k`, `k3.1-pro`, `kimi-k3`,
/// `kimi-k3.1-pro-highspeed`). Their request format differs from K2.7's: effort
/// is a top-level `reasoning_effort`, thinking cannot be turned off, and the
/// complete assistant message must come back.
fn is_kimi_k3(wire: &str) -> bool {
    let id = wire.strip_prefix("kimi-").unwrap_or(wire);
    id == "k3" || id.starts_with("k3-") || id.starts_with("k3.")
}

/// K2.7 Code and later, by documented model id.
///
/// The list is deliberately explicit. The product supports K2.7 and later
/// only, and the wire ids above that line do not share a prefix with the ones
/// below it (`kimi-k2-0905-preview` is an earlier model, and comparing version
/// strings would be a guess about an alias the provider re-maps in place). An
/// id that is not listed keeps the conservative default instead of inheriting
/// the requirement of a model it may not be.
///
/// `kimi-for-coding` / `kimi-for-coding-highspeed` are the Kimi Code ids: at the
/// time of writing they resolve to K2.8 Preview and K2.7 Code Highspeed, both
/// of which are above the supported line.
fn is_kimi_k27(wire: &str) -> bool {
    let id = wire.strip_prefix("kimi-").unwrap_or(wire);
    matches!(
        id,
        "k2.7-code"
            | "k2.7-code-highspeed"
            | "k2.8-preview"
            | "for-coding"
            | "for-coding-highspeed"
    ) || id.starts_with("k2.7-")
        || id.starts_with("k2.8-")
}

/// The protocol requirement this route's model family imposes, if any.
///
/// Only the OpenAI-Chat wire has a `reasoning_content` assistant field. The
/// Messages protocol carries authenticated thinking blocks instead, and the
/// Responses/Gemini adapters do not exist in this build, so a family fact never
/// invents a channel on those protocols.
///
/// This entry point answers the family's unconditional form, i.e. without a
/// preserved-thinking declaration. The model profile's
/// `capabilities.reasoning` flag is deliberately NOT consulted: every listed
/// family thinks by protocol (DeepSeek by server-side default, Kimi K2.7+ and
/// K3 unconditionally), so a config that forgot to declare reasoning would
/// otherwise be exactly the config that sends a request the provider rejects.
/// Every runtime path resolves through [`required_reasoning_replay_for`], which
/// also reads the declared preserved mode.
pub fn required_reasoning_replay(
    protocol: ProtocolKind,
    model_id: &str,
) -> Option<RequiredReasoningReplay> {
    required_replay_for_route_with(protocol, model_id, None)
}

/// The family requirement for a route, given the preserved-thinking mode the
/// route declared. Only GLM's rule reads it: the other families have no request
/// field for the mode and their requirements do not depend on it.
fn required_replay_for_route_with(
    protocol: ProtocolKind,
    model_id: &str,
    preserves_thinking: Option<bool>,
) -> Option<RequiredReasoningReplay> {
    if protocol != ProtocolKind::OpenAiChat {
        return None;
    }
    let family = wire_family(model_id)?;
    // GLM documents replay as the tool-loop default, but preserved thinking is
    // the mode that states the condition: while the route asks the endpoint to
    // keep prior turns' thinking, every request owes it back. That is what
    // makes the rule binding (and lets a deployment that turned preservation
    // off opt out).
    let glm_preserves = preserves_thinking == Some(true);
    let (scope, missing, enforcement) = match family {
        WireReasoningFamily::DeepSeek => (
            ReasoningReplayScope::WhenToolsPresent,
            MissingReasoningReplay::EmptyString,
            ReasoningReplayEnforcement::Required,
        ),
        WireReasoningFamily::Glm => (
            if glm_preserves {
                ReasoningReplayScope::Always
            } else {
                ReasoningReplayScope::WhenToolsPresent
            },
            MissingReasoningReplay::Omit,
            if glm_preserves {
                ReasoningReplayEnforcement::Required
            } else {
                ReasoningReplayEnforcement::Default
            },
        ),
        // K2.7+ and K3/K3.1 always think and always preserve. The complete
        // assistant message comes back on every request, and a turn that
        // captured none carries the legal empty value rather than a missing key
        // the K2.x line validates.
        WireReasoningFamily::KimiK3 | WireReasoningFamily::KimiK27 => (
            ReasoningReplayScope::Always,
            MissingReasoningReplay::EmptyString,
            ReasoningReplayEnforcement::Required,
        ),
    };
    Some(RequiredReasoningReplay {
        scope,
        missing,
        enforcement,
    })
}

/// The family requirement for one configured route.
pub fn required_reasoning_replay_for(profile: &ModelProfile) -> Option<RequiredReasoningReplay> {
    required_replay_for_route_with(
        profile.protocol,
        &profile.model_id,
        profile.reasoning.preserved_thinking,
    )
}

/// How this route's wire spells preserved thinking.
///
/// Keyed on the wire model id for the same reason the replay table is: the same
/// model served through another alias is still the same model family. A family
/// absent from this table has no preserved-thinking field at all — either
/// because it keeps nothing between requests (DeepSeek) or because preservation
/// is unconditional there (Kimi K2.7+/K3).
pub fn preserved_thinking_wire(protocol: ProtocolKind, model_id: &str) -> PreservedThinkingWire {
    if protocol != ProtocolKind::OpenAiChat {
        return PreservedThinkingWire::None;
    }
    match wire_family(model_id) {
        Some(WireReasoningFamily::Glm) => PreservedThinkingWire::ClearThinking,
        Some(WireReasoningFamily::DeepSeek)
        | Some(WireReasoningFamily::KimiK3 | WireReasoningFamily::KimiK27)
        | None => PreservedThinkingWire::None,
    }
}

impl WireReasoningFamily {
    /// Whether this family's request shape has a `thinking` object at all.
    ///
    /// DeepSeek and GLM spell thinking (and GLM preservation) with it. Kimi
    /// K2.7 and later and K3/K3.1 have none: K2.7 Code is always thinking and
    /// takes no parameter for it, and K3/K3.1 address reasoning with a
    /// top-level `reasoning_effort`. An optional field that the endpoint
    /// validates as `{type: "enabled", keep: "all"}` or nothing is not a
    /// knob, and sending `{type: "enabled"}` is a request the vendor says not
    /// to make.
    const fn takes_thinking_field(self) -> bool {
        match self {
            Self::DeepSeek | Self::Glm => true,
            Self::KimiK3 | Self::KimiK27 => false,
        }
    }
}

/// Whether a route's wire defines a `thinking` request object.
///
/// The encoder asks this before emitting one, so a route whose family has no
/// such field cannot receive another family's spelling even when its config
/// declares a style that would.
pub fn accepts_thinking_field(protocol: ProtocolKind, model_id: &str) -> bool {
    if protocol != ProtocolKind::OpenAiChat {
        return true;
    }
    wire_family(model_id).is_none_or(|family| family.takes_thinking_field())
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
        if !accepts_thinking_field(profile.protocol, &profile.model_id)
            && matches!(profile.reasoning.style, ReasoningStyle::ThinkingFlag)
        {
            // The family defines no `thinking` object. A declaration that
            // spells one would either be an illegal request or be silently
            // dropped by the encoder; neither is what the config asked for, so
            // the contradiction is reported instead of absorbed.
            return Err(ReasoningRouteConflict {
                model_id: profile.model_id.clone(),
                detail: "this wire family takes no `thinking` request field — its thinking \
                         (and, on K2.7 and later, its preservation) is unconditional, and \
                         K3/K3.1 address reasoning with a top-level `reasoning_effort`; \
                         `reasoning_style = thinking_flag` cannot be honoured here, so use \
                         `open_ai_effort` (or omit the style when the route has no effort knob)"
                    .into(),
            });
        }

        let preserved = profile.reasoning.preserved_thinking;
        if preserved.is_some()
            && preserved_thinking_wire(profile.protocol, &profile.model_id)
                == PreservedThinkingWire::None
        {
            // Accepting the declaration here would either send another
            // endpoint's field or do nothing at all — both are silent lies about
            // what the request asks for.
            return Err(ReasoningRouteConflict {
                model_id: profile.model_id.clone(),
                detail: "this wire family has no preserved-thinking request field — either \
                         the endpoint keeps nothing between requests, preservation is \
                         unconditional and cannot be asked for, or the route is not a \
                         supported model — so `preserved_thinking` cannot be honoured; \
                         remove the declaration"
                    .into(),
            });
        }

        let Some(required) = required_reasoning_replay_for(profile) else {
            return Ok(());
        };

        if preserved == Some(true)
            && required.enforcement == ReasoningReplayEnforcement::Required
            && let Some(declared) = profile.compatibility.reasoning_replay_scope
            && replay_rank(declared) < replay_rank(required.scope)
        {
            return Err(ReasoningRouteConflict {
                model_id: profile.model_id.clone(),
                detail: format!(
                    "`preserved_thinking = true` asks this endpoint to keep its previous \
                     turns' thinking, which requires returning the complete, unmodified \
                     reasoning of every historical assistant turn, but \
                     `reasoning_replay_scope = {}` returns none; set `preserved_thinking = \
                     false` so the request actually clears prior thinking, or remove the \
                     replay override",
                    declared.as_str()
                ),
            });
        }

        if required.enforcement == ReasoningReplayEnforcement::Required
            && let Some(declared) = profile.compatibility.reasoning_replay_scope
            && replay_rank(declared) < replay_rank(required.scope)
        {
            return Err(ReasoningRouteConflict {
                model_id: profile.model_id.clone(),
                detail: format!(
                    "this wire protocol requires historical reasoning replay {}, but \
                     `reasoning_replay_scope = {}` returns less; the provider rejects such a \
                     request with HTTP 400 — remove the override or declare `{}`/`always`",
                    match required.scope {
                        ReasoningReplayScope::Always => "on every request",
                        _ => "on every tool-bearing request",
                    },
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
