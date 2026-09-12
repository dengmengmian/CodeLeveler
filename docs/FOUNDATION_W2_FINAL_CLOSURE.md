# Foundation W2 Final Closure — Browser and web_search Documentation Truth

## 1. Status

```text
FOUNDATION_W2_FINAL_CLOSURE=PASS
```

W2 asked one question: does the documentation describe the Browser and
`web_search` capabilities that the code actually ships? It now does. The
contracts were re-read against the current tree, four targeted test suites were
re-run on the tested baseline, and the README and Architecture documents in both
languages were brought onto those facts.

This closure is documentation only. No product behaviour was changed by it.

## 2. Scope

- Re-verify the Browser tool surface, its action ownership, and browser product
  selection against the current code and tests.
- Re-verify the `web_search` capability: availability, exposure, public schema,
  and its boundary with the browser.
- Align `README.md`, `README.zh-CN.md`, `docs/ARCHITECTURE.md` and
  `docs/ARCHITECTURE.zh-CN.md` with those contracts, with the two languages
  saying the same thing.
- Record the mechanical evidence, and record what that evidence does *not*
  prove.

## 3. Non-scope

Nothing below was touched, and nothing below is claimed to be resolved:

- Browser runtime behaviour.
- `web_search` runtime behaviour.
- The Windows Job Object canaries, their instrumentation, or their time budget.
- The Windows canary setup root cause.
- The daemon crash-consistency E2E race.
- Engine ↔ Coding Harness decoupling.
- W3, W4, and any architectural optimisation.

## 4. Tested Identity

```text
TESTED_HEAD=0f3d26fb5a9554def8c74f87dadb5cd2c9a51b22
BASE_CI_RUN_ID=34695945639
BASE_CI_COMMIT=0f3d26fb5a9554def8c74f87dadb5cd2c9a51b22
BASE_CI_RUN_ATTEMPT=1
BASE_CI_FINAL_RESULT=PASS
BASE_CI_FIRST_ATTEMPT_STABLE=YES
BASE_CI_EXACT_COMMIT=YES
```

`TESTED_HEAD` is the product baseline the contracts and tests were verified
against. The commit that carries this document is a later, documentation-only
commit and is deliberately not written here: a closure document cannot contain
its own hash, and a docs commit is not a tested product baseline.

## 5. Browser Contract

```text
BROWSER_PUBLIC_TOOL_COUNT=3
BROWSER_PUBLIC_TOOLS=browser_tab,browser_act,browser_inspect
BROWSER_SELECTION_PRECEDENCE=explicit>configured>system_default
BROWSER_SILENT_SUBSTITUTION=FORBIDDEN
BROWSER_CAPABILITY_COMPOSITION=PASS
BROWSER_DOCUMENTATION_TRUTH=PASS
```

Three tools reach the model, and each owns one job:

| Tool | Owns |
| --- | --- |
| `browser_tab` | `navigate`, `snapshot`, `screenshot`, `reload`, `list_tabs`, `new_tab`, `select_tab`, `close_tab`; and the optional `browser` argument that names a product |
| `browser_act` | `click`, `fill`, `type`, `press`, `select`, `scroll` |
| `browser_inspect` | `console`, `page_errors`, `network` via its `what` enum |

No action appears in two tools. `browser_surface.rs` asserts the disjointness
directly, so an `action=`-shaped door from one tool onto another cannot grow
back unnoticed.

Selection is a precedence, not a search for anything that works:

```text
named on the call  >  [browser].default  >  system default browser
```

A selected product that cannot be driven returns `BrowserError::Unavailable`
naming that product and the layer that chose it. No installed browser is
substituted. Chrome and Edge remain distinct products although both are driven
over CDP, and the launcher distinguishes CDP from WebDriver — a product is not a
protocol. Safari is available only on macOS, and only when `safaridriver`
exists; its remote-automation setting lives inside a TCC-protected container
this process cannot read, so the definitive answer comes from `safaridriver` at
launch.

Capability availability asks whether the browser this host *would select* is
drivable, not whether any browser is installed
(`Browser::resolve(None).is_ok()`). A user whose selected browser cannot be
driven therefore sees no browser tools at all rather than a surprising one.

## 6. web_search Contract

```text
WEB_SEARCH_OPTIONAL_CAPABILITY=YES
WEB_SEARCH_EXPOSURE=ENABLED_INTERSECT_AVAILABLE
WEB_SEARCH_KEY_ENV=LEVELER_SEARCH_API_KEY
WEB_SEARCH_SCHEMA=query,count
WEB_SEARCH_DEFAULT_COUNT=5
WEB_SEARCH_MAX_COUNT=10
WEB_SEARCH_ECONOMY_EXPOSED=NO
WEB_SEARCH_BROWSER_FALLBACK=NO
WEB_SEARCH_DOCUMENTATION_TRUTH=PASS
```

The key is read in exactly one place, from `LEVELER_SEARCH_API_KEY`, trimmed,
and a blank value is not a configuration. Without a key the tool is never
registered — there is no "not configured" branch inside `execute`, because that
state cannot reach it. The product does not advertise a capability whose only
possible answer is that it was never set up.

The model's contract is `query` plus `count`, with `count` clamped to `[1, 10]`
and defaulting to 5. Results are titles, URLs and snippets. One request, a
ten-second timeout, no retry and no fallback to another capability. Network
denial by the active mode or sandbox is answered with an error rather than
bypassed — for an in-process HTTP client this check is the only enforcement
point, since the OS sandbox covers `run_command` children.

Tavily is the current backend. That is an implementation fact, recorded in
Architecture; the tool contract stays provider-neutral, and provider-internal
fields such as per-result `score` and `response_time` are asserted never to
reach the model.

## 7. Capability Composition

```text
EXPOSED = AVAILABLE ∩ ENABLED
```

- **AVAILABLE** — what this machine can provide. Mechanical: is a search key
  configured, is `git` on `PATH`, does this model accept images, is the selected
  browser drivable.
- **ENABLED** — what the product mode asks for. `Economy` maps to
  `CapabilityPacks::NONE`; `Balanced` and `Delivery` map to
  `CapabilityPacks::ALL`.
- **EXPOSED** — their intersection, and a boundary rather than a suggestion:
  neither side can widen the other.

A configured search key is therefore still invisible under `Economy`, and an
enabled pack the host cannot back exposes nothing. Browser control and
`web_search` are separate capabilities, configured separately, and neither is
the other's fallback.

## 8. Mechanical Evidence

All four suites were re-run on `TESTED_HEAD` with `--all-features --locked`.

| Command | Ran | Passed | Failed | Proves | Does not prove |
| --- | --- | --- | --- | --- | --- |
| `cargo test -p leveler-tools --test browser_surface` | 4 | 4 | 0 | The model-visible browser surface is exactly three tools, with disjoint action ownership and no procedural advice in the descriptions | That any user machine has a drivable browser installed |
| `cargo test -p leveler-tools --test capability_composition` | 7 | 7 | 0 | `EXPOSED = ENABLED ∩ AVAILABLE`, its symmetry, that neither side widens the other, that an Economy turn hides a configured key, and that the `web_search` schema stays `query,count` | That an external search service is reachable or permanently available |
| `cargo test -p leveler-browser --test selection` | 4 | 4 | 0 | `explicit > configured > system default` on a real host, that an absent configured product is an `Unavailable` error naming it, and that Chrome and Edge never resolve to each other | That every platform has every browser product installed |
| `cargo test -p leveler-tools web_search` | 6 | 6 | 0 | The result format, the request body, the `count` cap, and that provider-internal fields never reach the model | That a live Tavily request succeeded — no network call is made by these tests |

```text
ZERO_TEST_FILTER_ACCEPTED=NO
```

The `web_search` filter is a name filter, so it also matches two tests inside
`capability_composition` (both passed) and reports `running 0 tests` for the
test binaries where nothing matches. The six counted above are the
`tools::web_search::tests` module in the `leveler-tools` library unit tests,
with 288 tests filtered out. No `running 0 tests` line is used as evidence for
anything.

## 9. Base CI Evidence

Run `34695945639`, `head_sha=0f3d26fb5a9554def8c74f87dadb5cd2c9a51b22`,
`head_branch=main`, `event=push`, `status=completed`, `conclusion=success`,
`run_attempt=1`. All six jobs succeeded on the first attempt: `deny · audit`,
the three `fmt · clippy · test` matrix jobs, `leveler-web UI`, and
`leveler-mobile`.

Browser acceptance was read from the job logs rather than from step
conclusions:

| Platform | Product | Tests run | Result |
| --- | --- | --- | --- |
| `windows-latest` | Edge | 12 | `test result: ok. 12 passed; 0 failed` — PASS |
| `macos-latest` | Chrome | 12 | `test result: ok. 12 passed; 0 failed` — PASS |
| `ubuntu-latest` | Chrome | 12 | `test result: ok. 12 passed; 0 failed` — PASS |

```text
WINDOWS_EDGE_ACCEPTANCE=PASS
MACOS_CHROME_ACCEPTANCE=PASS
LINUX_CHROME_ACCEPTANCE=PASS
```

Each platform ran the suite that its own product is gated to and skipped the
other; no acceptance step was a silent no-op. These runs prove the capability
works against a real browser on a CI runner. They do not prove that an
arbitrary user machine has a drivable browser — that is what capability
availability is computed for, per turn.

## 10. Documentation Changes

| File | Change |
| --- | --- |
| `README.md` | New "Optional capabilities" section: the exposure rule, the three browser tools and their jobs, the selection precedence table, no silent substitution, and the `web_search` key / schema / no-fallback facts |
| `README.zh-CN.md` | The same section in Chinese, semantically identical |
| `docs/ARCHITECTURE.md` | §8.1 available / enabled / exposed as three distinct facts; §8.2 browser control and web search as separate capabilities, with the selection precedence and the provider-neutral tool contract |
| `docs/ARCHITECTURE.zh-CN.md` | §8.1 and §8.2 in Chinese, semantically identical |
| `docs/FOUNDATION_W2_FINAL_CLOSURE.md` | This document |

One claim carried by the draft patch was dropped during re-audit: it said
`leveler doctor` reports what this machine can provide as an optional
capability. `leveler_app::doctor` has no browser check and no search-key check —
it reports tooling on `PATH`, the model bundle, reasoning, global config,
per-provider API keys, MCP env references, the sandbox line and the memory
store. The sentence was removed from both READMEs rather than shipped. The
pre-existing `leveler doctor` sentence in the platform-support section is about
sandbox capabilities, which doctor does report, and was left alone.

```text
ACTUAL_DIFF_REVIEWED=YES
DOCS_ONLY_DIFF=YES
BILINGUAL_DOCUMENTATION_PARITY=YES
DOCUMENTATION_TRUTH_ALIGNED=YES
```

## 11. Explicit Non-claims

- Browser acceptance passing on CI runners is **not** a claim that every user
  machine can drive a browser.
- Browser acceptance is **not** `web_search` live acceptance. No test in this
  closure makes a real search request.
- The browser is **not** a fallback for `web_search`, and `web_search` is
  **not** a fallback for the browser.
- `web_fetch` and `web_search` are different tools with different contracts.
- `web_search` does **not** retry and does **not** switch provider.
- Provider-internal fields are **not** part of any public contract.
- A single green Windows canary run is **not** proof of determinism.
- A rerun-free green base CI is **not** a claim that the known daemon race is
  fixed.

## 12. Known Residuals

### Windows Job Object canary setup

```text
KNOWN_WINDOWS_CANARY_SETUP_INTERMITTENCY=YES
WINDOWS_CANARY_DIAGNOSTICS_PRESENT=YES
WINDOWS_CANARY_DIAGNOSTIC_OBSERVABILITY=PASS
WINDOWS_CANARY_SETUP_ROOT_CAUSE=UNDETERMINED
WINDOWS_CANARY_SETUP_DETERMINISM=NOT_PROVEN
FIXED_IN_W2=NO
```

Both canaries passed in run `34695945639`, and the diagnostics added before this
closure did their job: the logs now state when the fixture's pid file became
readable and when the alive witness was established
(`3.24s` / `3.26s` and `3.73s` / `3.73s` respectively). That is observability,
not a fix. One green run with a comfortable margin does not establish why the
setup was intermittent, and does not prove it cannot exceed its budget again.
The instrumentation makes the next failure legible; it did not remove the race.

### Daemon crash-consistency E2E race

```text
KNOWN_DAEMON_E2E_RACE_OPEN=YES
TEST_NAME=sigkill_during_a_task_recovers_on_restart_without_duplication
TEST_LOCATION=crates/leveler-cli/tests/daemon_e2e.rs
OBSERVED_FAILURE=marker_count expected 1, observed 0
FIXED_IN_W2=NO
```

The test waits until a turn row is durably `running`, SIGKILLs the daemon,
restarts it, and then asserts the initiating user message is durable exactly
once. Those are two independent persistence boundaries, and the test treats the
first as a witness for the second. Whether the product owes the guarantee

```text
durable running turn  ⇒  durable initiating user message
```

or whether the test picked the wrong witness has not been decided
mechanically. Until it is, this is an open crash-consistency question, not a
harmless flake and not a proven product defect.

## 13. Final Gates

```text
TESTED_HEAD=0f3d26fb5a9554def8c74f87dadb5cd2c9a51b22
BASE_CI_EXACT_COMMIT=YES
BASE_CI_FINAL_RESULT=PASS
BASE_CI_FIRST_ATTEMPT_STABLE=YES

BROWSER_CONTRACT_VERIFIED=YES
WEB_SEARCH_CONTRACT_VERIFIED=YES
CAPABILITY_COMPOSITION_VERIFIED=YES
DOCUMENTATION_TRUTH_ALIGNED=YES

PRODUCT_CODE_CHANGED=NO
TEST_CODE_CHANGED=NO
CI_CHANGED=NO
GENERATED_FILES_CHANGED=NO
DEPENDENCY_CHANGED=NO
CONFIG_CHANGED=NO
TIMING_CHANGED=NO

FOUNDATION_W2_FINAL_CLOSURE=PASS

KNOWN_WINDOWS_CANARY_SETUP_INTERMITTENCY=YES
WINDOWS_CANARY_SETUP_ROOT_CAUSE=UNDETERMINED
WINDOWS_CANARY_SETUP_DETERMINISM=NOT_PROVEN
KNOWN_DAEMON_E2E_RACE_OPEN=YES

READY_FOR_DAEMON_RACE_CLOSURE=YES
READY_FOR_W3=NO
W3_STARTED=NO
FOUNDATION_FROZEN=NO
```

## 14. Handoff

The next task is fixed, and it is not W3:

**Daemon Crash-Consistency Race Closure.** Decide mechanically whether

```text
durable running turn  ⇒  durable initiating user message
```

is a guarantee the product owes, and then either make it hold or correct the
test's witness. `READY_FOR_W3` stays `NO` until that closes.
