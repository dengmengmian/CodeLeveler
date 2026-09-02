# Reasoning Capability Audit

Verified: 2026-08-18 against this tree and official provider docs
(not blogs). Built-in models are only those under `configs/models/`.

## 1. Current architecture

```
configs/models/*.yaml  +  ~/.leveler/config.toml
        ↓
ModelProfile.reasoning: ReasoningConfig { style, effort: Option }
        ↓
resolve_execution_policy: override.or(profile.reasoning.effort)
        ↓
Executor.with_reasoning_effort → ModelRequest.reasoning_effort
        ↓
openai_chat encode:
  request.reasoning_effort.or(context.reasoning.effort)
  style None        → send nothing
  style OpenAiEffort → reasoning_effort
  style ThinkingFlag → thinking.enabled + reasoning_effort
        ↓
TUI AppState.reasoning_effort
  boot from GlobalConfig / LoadedConfig profile.effort
  cleared on /model switch
  SessionUpdated does not currently carry effort
```

`effort` on the profile is both “what to send if nobody asked” and
the only model-side fact. There is no `supported_efforts`. The
protocol adapter still applies a second fallback. The TUI does not
receive a runtime-projected effective value after a live model switch.

## 2. Existing problems (from code, not assumed)

1. **Capability and policy are the same field.** `ReasoningConfig.effort`
   is a recommended default, not a capability set. Nothing records
   that DeepSeek/K3 only accept `low|high|max`.
2. **No validation.** `default=medium` + native `{high,max}` would load.
3. **Adapter still decides policy.**
   `request.reasoning_effort.or(context.reasoning.effort)` in
   `openai_chat`.
4. **Two built-in reasoning models have no CodeLeveler effort at all:**
   `kimi-for-coding` and `kimi-for-coding-highspeed` (`style: none`).
   That is honest for the wire (always-on thinking, no knob) but the
   UI/resolver cannot name an effective effort.
5. **Protocol adapter does not normalize.** A request of `medium` is
   sent as `"medium"` even when the model only accepts `low|high|max`.
   DeepSeek’s own docs remap `medium`/`xhigh` → `high`; we do not.
6. **TUI is not a runtime projection.** Effort comes from boot-time
   config lookup and is wiped on `/model` until restart.
7. **`XHigh` is missing** from the canonical enum (OpenAI / some Claude).
8. **Anthropic encode does not send thinking/effort** (decode only).
   `openai_responses` / `gemini_generate_content` parse as protocol
   kinds but fail at startup (named error).
9. **Doctor does not print** supported / default / effective effort.

What is already correct:

- Canonical `ReasoningEffort` + `ReasoningStyle` as wire *mechanism*
  (not brand) is the right split.
- `Option<ReasoningEffort>::None` already means “no explicit request”,
  not “disable thinking”.
- DeepSeek/K3 profiles already pick an explicit CodeLeveler default
  (`max`) instead of omitting the field.
- Forced `tool_choice` + thinking disable is a compatibility quirk,
  not a default-effort policy. Keep it.

## 3. Provider capability matrix (official)

| Model | Provider | Reasoning | Style | Native efforts | Provider default | Evidence |
| --- | --- | --- | --- | --- | --- | --- |
| deepseek-v4-pro | deepseek | yes | thinking_flag | low, high, max | **high** (thinking on by default) | https://api-docs.deepseek.com/guides/thinking_mode/ (2026-08-18). Compatibility: medium→high, xhigh→high. |
| deepseek-v4-flash | deepseek | yes | thinking_flag | low, high, max | **high** | same page, identical mapping |
| k3 | kimi | yes | openai_effort | low, high, max | **high** (`null`→high; unknown→HTTP 400) | https://www.kimi.com/code/docs/kimi-code/models (2026-08-18). Also: ultra/max/xhigh→max; medium→high; none disables thinking and **routes to K2.6**. |
| kimi-for-coding | kimi | yes | none | *no effort knob* | thinking **ON** (no documented level) | same page: `Thinking:ON`. Closing thinking routes to K2.6. |
| kimi-for-coding-highspeed | kimi | yes | none | *no effort knob* | thinking **ON** | same |

Not in `configs/models/` (no built-in profile; gaps only):

| Family | Native / wire | Provider default | CodeLeveler |
| --- | --- | --- | --- |
| OpenAI reasoning | `reasoning.effort` / `reasoning_effort`: model-dependent `none\|minimal\|low\|medium\|high\|xhigh\|max` | **model-dependent** (e.g. gpt-5.5 → medium) | https://developers.openai.com/api/docs/guides/reasoning |
| Anthropic | `output_config.effort` + `thinking`; levels vary (`low…max`) | model-dependent (Opus 5 docs: start at **high**) | encode **not implemented** |
| Gemini 3 | `thinking_level` (not `thinking_budget` on 3.x) | **per model** (3.1 Pro high; 3.7 Flash medium) | protocol kind exists, adapter **not implemented** |
| GLM | not in built-in tree | — | no profile |

## 4. Default matrix (must stay two columns)

| Model | Provider default | CodeLeveler default (product) |
| --- | --- | --- |
| deepseek-v4-pro | high | **max** (already in yaml; coding-agent choice) |
| deepseek-v4-flash | high | **max** |
| k3 | high | **max** (yaml today; official table default is high — we keep max as *our* policy) |
| kimi-for-coding | thinking ON, effort N/A | **N/A** (no knob; do not invent `high`) |
| kimi-for-coding-highspeed | thinking ON, effort N/A | **N/A** |

CodeLeveler default ≠ provider default is allowed. Pretending the
vendor default is `max` is not.

## 5. Proposed architecture

Keep the canonical enum. Add `XHigh`. Do **not** add `None` as a
level.

```
ReasoningConfig
  style
  supported_efforts   // capability
  default_effort      // CodeLeveler policy (serde alias: effort)

resolve_reasoning_effort(requested, config) -> ResolvedReasoning
  requested / default / effective / source

ModelProfile validation at load
  reasoning && style != None
    → supported non-empty, default present, default ∈ supported
  reasoning && style == None
    → no controllable effort (always-on or no knob)
    → supported empty, default absent
  !reasoning
    → no supported, no default

Protocol adapter
  encodes effective from ModelRequest only
  does not remap medium→high

Client protocol
  UiSessionSnapshot.reasoning_effort: Option<String>  // additive
  TUI copies it; never infers from model name
```

Normalization (when the model lists supported efforts):

```
minimal < low < medium < high < xhigh < max
requested → smallest supported ≥ requested
if none → clamp to the model's max supported
```

DeepSeek/Kimi compatibility aliases (`medium`→`high` on their
servers) are **not** implemented in the adapter. After our
resolver, `medium` against `{low,high,max}` becomes `high` and
that is what we send. `xhigh` against that set becomes `max`
(upgrade). Official DeepSeek remap of *wire* `xhigh`→`high` does
not apply because we never send `xhigh` to DeepSeek.

## 6. Migration impact

- Old yaml `reasoning.effort: max` still loads (`alias = "effort"`).
- If only `effort` is set and `supported_efforts` is empty, load
  infers `supported = [default]` so existing user TOML keeps working.
- Built-in files will be rewritten with explicit lists.
- Snapshot field is `#[serde(default)]` — old runtimes / old clients
  stay compatible.
- Users do not have to change config.

## 7. Implementation plan

| File | Change |
| --- | --- |
| `leveler-model` `profile.rs` | `XHigh`, `supported_efforts`, `default_effort`, resolve + validate |
| `leveler-provider` `catalog.rs` | validate on load + contract test over `configs/models/*` |
| `leveler-engine` `policy_resolver.rs` | call resolver |
| `leveler-protocol` `openai_chat` | encode request effort only |
| `configs/models/*.yaml` | explicit supported + default + evidence comments |
| `leveler-client-protocol` snapshot | additive `reasoning_effort` |
| `leveler-app` interactive + doctor | project effective; print matrix |
| `leveler-tui` `apply_meta` | copy snapshot effort |
| `global_config.rs` / `example.yaml` | map new fields; document two defaults |

Out of scope (capability gaps, not silent pretence):

- Anthropic `output_config.effort` encode
- Gemini `thinking_level` adapter
- OpenAI Responses protocol

## 8. Implementation status (2026-08-18)

Shipped in this tree:

- Canonical `ReasoningEffort` includes `XHigh`. `None` is still
  `Option::None` (no request), not a level.
- `ReasoningConfig` splits `supported_efforts` (capability) from
  `default_effort` (CodeLeveler policy; serde alias `effort`).
- `resolve_reasoning_effort` is the single resolver. Policy, CLI boot,
  and diagnostics call it. The OpenAI Chat adapter encodes
  `ModelRequest.reasoning_effort` only.
- Built-in yaml files declare supported + default, or `style: none`
  for always-on no-knob models.
- `load_model_config` and `~/.leveler/config.toml` validate.
- `UiSessionSnapshot.reasoning` is additive; TUI copies `effective`
  and never infers from the model name.
- `leveler models show` / `leveler doctor` print the matrix.
- Contract test walks `configs/models/*`.

## 9. Open question: the pass-back reasoning chain is always empty (2026-09-02)

Found while auditing token cost, not fixed here — the right answer needs
a measurement this audit cannot supply.

**The mechanism.** `configs/models/deepseek-v4-pro.yaml` and
`deepseek-v4-flash.yaml` both set `compatibility.passback_reasoning_content:
true`, so the OpenAI Chat adapter attaches `reasoning_content` to every
assistant message that carries tool calls (`openai_chat/mod.rs`, the
`passback_reasoning` branch). That is the contract DeepSeek's thinking mode
asks for.

**The gap.** The streaming path never stores what it received. `stream_round`
observes `ReasoningDelta` for the UI and drops it; the message it returns
holds only `Text` and `ToolCall` parts (`executor/stream.rs`). So the key is
always present and always `""`. The non-streaming `decode_response` does keep
`Reasoning` parts, but the drive loop does not use it.

Net effect: on a thinking model, every tool-calling round pays for reasoning
output tokens and then discards the chain before the next request. The wire
stays valid — DeepSeek accepts the empty string — so nothing fails loudly.

**Why this is not obviously a bug.** Re-sending the chain is not free: those
tokens re-enter the input of every subsequent request in the turn, and they
accumulate exactly like tool results do (see C2.1 §12). DeepSeek Harness
takes the opposite position — it replays `reasoning_content` in full and
relies solely on a 0.8-of-window compaction trigger, with no progressive
decay. Neither position has been measured here.

**What would settle it.** One ablation on the existing eval seam, thinking
model only, single variable:

| Arm | Assistant message keeps reasoning | Wire carries it |
| --- | --- | --- |
| control | no (today) | `reasoning_content: ""` |
| ablated | yes, for the current tool loop | the captured chain |

Report per arm: completion rate, rounds to first edit, provider
`input_tokens` and `cached_input_tokens` totals, and output tokens. The
question is whether the chain buys enough continuity to pay for its own
re-billing. If the arms tie, keep today's behaviour and delete the
pass-back flag from both profiles rather than leaving a contract that
carries nothing.

**Status (2026-09-02): both arms exist.** `stream_round` keeps the chain on
the assistant message that produced it when `TurnPolicy::keep_reasoning` is
set, so the pass-back key carries what the provider asked for instead of `""`.
Default off — today's behaviour — and reachable through the `keep_reasoning`
eval knob. The parts are ordered reasoning-then-text so the OpenAI-chat
encoder, which joins by kind, presents the chain as preceding the answer it
produced.

**Measured (2026-09-03): sending the chain back costs 34 % more input and
buys nothing.** Frozen `dcb51f4d7ee5`, v4-flash, `scale-s800`, one repetition
per arm: input 573,329 → 765,726, output 19,325 → 31,128, rounds 19 → 22,
first edit round 6 → 11, cost $0.085 → $0.115, acceptance unchanged (both
failed). Evidence:
`DOGFOOD_ROOT/eval/state/f7-followup-dcb51f4d7ee5/REPORT.md`.

So today's behaviour — show the chain, drop it — is the cheaper position on
this evidence, and DeepSeek Harness's full replay is the more expensive one.
`keep_reasoning` stays off.

What remains open is the flag, not the behaviour: `passback_reasoning_content`
on both profiles now declares a contract that carries `""` and, on this
measurement, should carry nothing. Removing it is a product decision; one
repetition of one case does not license deleting a provider contract.
