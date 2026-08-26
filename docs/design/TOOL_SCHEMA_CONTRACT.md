# Tool schema contract

**Status:** invariant + audit · 2026-08-26

What a published tool schema must say, why, and the audit that found the one
place it was lying.

## The invariant

> **Published tool schemas MUST express every unconditional structural
> requirement enforced by runtime validation. Runtime validation MAY enforce
> additional semantic or conditional constraints.**

Two gates, not one:

```
Schema      first gate   — structural, machine-enforced by strict providers
Runtime     last gate    — semantic and conditional, always runs
```

The failure this invariant prevents is the schema being *more permissive* than
the runtime. When that happens the model is contractually allowed to send
something the runtime will refuse, and it is not the model's fault.

## What went wrong in `run_command`

Three layers disagreed about whether `program` was required.

| Layer | Said | Enforced by |
| --- | --- | --- |
| JSON Schema `required` | **optional** | strict function-calling providers |
| Field description (prose) | "Required in practice, but kept schema-optional…" | nothing |
| Runtime validation | required | always |

A real session produced this, twice, and it was legal under the published
contract:

```json
{"args":["test","./..."],"cwd":"."}
```

No `program`. The model split the command correctly and simply omitted a field
the schema told it was optional.

### Two distinct defects

**1. The machine contract was wrong.** `Option<String>` + `#[serde(default)]`
makes schemars omit the field from `required`. The type was written that way
deliberately — so a missing value reaches `execute` and gets a helpful message
instead of a bare schema rejection — but that traded a machine-enforceable
contract for error ergonomics. The schema is the strongest signal available and
should not be spent on wording.

**2. Implementation rationale leaked into the model-facing description.**
schemars publishes doc comments as the schema `description`, so the model read:

> "The program to run, e.g. "cargo". **Required in practice, but kept
> schema-optional** so a missing/blank value reaches the tool…"

That is an explanation for maintainers, published to the model, and it reads as
permission.

### The fix keeps all three layers, correctly aligned

```rust
/// Required. The executable only, e.g. "cargo" or "go" — never a whole
/// command line. Everything after it goes in `args`.
//
// `schemars(required)` keeps the MACHINE contract honest… The `Option`
// remains so a provider that does not enforce `required` still reaches
// `execute` and gets a message naming what is missing.
#[serde(default)]
#[schemars(required)]
program: Option<String>,
```

| Attribute | Buys |
| --- | --- |
| `#[schemars(required)]` | strict providers must supply it |
| `Option<String>` | non-strict providers still reach runtime validation |
| `#[serde(default)]` | a missing field is not a bare deserialize error |

Not three alternatives — three layers.

### Published schema, before and after

```diff
  "required": [
+   "program"
  ]
```

### The error message was also wrong advice

The old message assumed the caller had crammed a shell string into `program`
and steered it to `shell_command`. For the observed payload that is the wrong
repair: the `args` array was already correct and only the executable was
missing. Switching tools "works" — the session did exactly that on its third
attempt — while leaving the caller none the wiser.

Now the two shapes get different guidance, and the repair leads:

```
run_command is missing `program`: the executable that runs `test ./...`.
`args` is correct as an array — it just has nothing to run. Repeat the call
with `program` set, e.g. {"program": "go", "args": ["test","./..."]}.
Use shell_command only for a whole command line with pipes, $(), redirection or &&.
```

**The executable is never guessed from `args`.** `{"args":["test","./..."]}`
most likely means `go test`, but running argv[0] would launch `/usr/bin/test`
instead — a different program, silently. Naming the missing field is honest;
inferring it is a coin flip with side effects.

## The second real failure — and what it proved

After the schema fix landed, a real session produced another program-less
call. Forensics (EventLog, verbatim):

```json
{"args":["test","./...","-count=1"],"timeout_seconds":120}
```

What the trace established, link by link:

| Link | Verdict |
| --- | --- |
| Stream parsing | strict JSON, never repairs — the arguments are the model's own, verbatim |
| Outbound conversion | `parameters: t.input_schema.clone()` — `required` reaches the wire untouched |
| Strict function calling | not supported by the provider, not requested by CodeLeveler — `required` is advisory to this model |
| Prompts/examples | no repo text teaches `cmd` or args-only shapes |
| **The binary** | **pre-fix.** The error text in the log is the old universal message, which the fixed code cannot produce for a non-empty `args` — the session ran against a long-lived daemon started before the fixed binary was installed |

So the schema fix was never actually exercised: whether it changes the
model's behavior is unproven until a real session runs on the fixed binary.
The failure did prove the model omits `program` habitually — which is why the
runtime fallback layer exists at all.

### The cmd shape is refused by name

`{"cmd": "go test ./..."}` on run_command deserializes with `cmd` dropped
(no `deny_unknown_fields`), so it used to fall into the generic empty-args
message. Now the raw payload is checked first and the refusal names the field:
the caller is told `cmd` was seen, nothing was executed, and which tool owns
that shape. **Never silently convert one tool into the other** — run_command
executes argv, shell_command executes a shell line; they differ in execution
and security semantics, and a silent conversion runs the caller's text under
a model it did not choose.

### The TUI must not prettify invalid calls

`execute_command_summary` used to render `{"args":["test","./..."]}` as
`$ test ./... -count=1` and `run_command {"cmd": …}` as `$ go test …` — a
valid-looking command line for a call the runtime refused (and naming the
wrong executable). Both now render `参数无效 · 缺少 program`, and the `$`
prefix is reserved for genuine command lines.

## Audit — every tool argument struct

Looking for the dangerous shape: `Option<T>` + unconditional runtime rejection
of `None` + not in the schema's `required`.

Method: for every `Option` field in every tool input struct (test modules
excluded), detect `let Some(x) = input.field … else`, `.ok_or`, and equivalent
unconditional-refusal forms, then check the field's schemars attributes.

| File | Field | Verdict |
| --- | --- | --- |
| `run_command.rs` | `program` | **real mismatch — fixed** |
| `browser.rs` | `ref_hidden` | false positive |
| `update_plan.rs` | `explanation` | false positive |

**`browser.rs::ref_hidden`** sits in an `else if` chain: one of
`url_contains` / `text_visible` / `text_gone` / `ref_visible` / `ref_hidden`
must be supplied. That is a *conditional* requirement — "at least one of these"
— and putting any single one in `required[]` would be wrong.

**`update_plan.rs::explanation`** is `if let Some(...)`: supplied, it is used;
absent, it is skipped. Genuinely optional. The scan mistook `if let` for a
refusal.

```
TOOL_SCHEMA_RUNTIME_REQUIRED_MISMATCH = 0
```

## Why there is no Contract Framework

One occurrence does not justify a `ToolRuntimeRequiredFields` /
`ToolSchemaRequiredFields` / `ContractValidator` layer. That design produces
three declarations of the same fact — schema, metadata, implementation — which
is the drift this invariant exists to prevent, reproduced at a larger scale.

The invariant plus a regression test on the one tool that broke it is the
proportionate response. Revisit if a second genuine mismatch appears.

## Regression tests

Asserted against `ToolRegistry::definitions()` — the exact value handed to a
provider — not the local type's schema. If a registry transform, adapter or
normalisation step ever drops `required`, a type-local test would still pass
while the contract was broken in production.

| Test | Locks |
| --- | --- |
| `the_published_schema_requires_program` | the published `required` contains `program` |
| `the_program_field_does_not_tell_the_model_it_is_optional` | no implementation rationale in the description |
| `a_missing_program_with_real_args_names_the_missing_field` | the repair leads; a concrete corrected call is shown |
| `a_shell_string_is_still_steered_to_shell_command` | the other shape keeps its correct advice |
| `an_empty_program_is_refused_by_the_runtime` | the last gate survives the first gate being fixed |
| `a_valid_call_still_runs` | well-formed calls are untouched |

## What is deliberately left to the runtime

Structural constraints belong in the schema; semantic ones do not.

| Constraint | Where | Why |
| --- | --- | --- |
| `program` present | schema | unconditional, structural |
| `program` non-empty after trim | runtime | `"   "` is a semantic judgement; encoding it as `minLength` would still miss it |
| browser: at least one predicate | runtime | conditional across fields, not expressible as `required[]` |

Chasing "everything in the schema" adds machinery without adding truth.
