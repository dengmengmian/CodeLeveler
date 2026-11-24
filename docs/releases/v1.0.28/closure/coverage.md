# Declared-gap audit

Source: `dogfood/eval/feature_inventory.yaml` as consumed by the v1.0.28 gate
(run `20261121-113252-c86573f209ca`): 317 features, 260 covered, 4 partial,
2 unverified, 50 declared gaps, 1 out of scope.

This audit verifies what can be executed on this host without inventing
evidence. Closing a gap here means adding process-level coverage going
forward; it does not retroactively change what the published v1.0.28 package
was verified to do.

## Closed by this closure (15 of 50)

All run the real `leveler` binary in a temporary `HOME`/`LEVELER_HOME` and
assert stdout, stderr, exit status and persisted files.

| Feature | Evidence |
| --- | --- |
| `cli.agents.show` | `crates/leveler-cli/tests/agents.rs:115` (text and unknown-name refusal) |
| `cli.skills.show` | `crates/leveler-cli/tests/skills.rs:145` |
| `cli.permissions.clear` | `crates/leveler-cli/tests/permissions.rs:20` |
| `cli.theme` | `cli_surface_contract.rs:118`, `:188` |
| `cli.models.show` | `cli_state_contract.rs:194` |
| `cli.sessions.show` | `cli_state_contract.rs:91` (text and JSON, unknown id exit 1) |
| `cli.sessions.rename` | `cli_state_contract.rs:129` (trimmed title, goal preserved, empty refused) |
| `cli.sessions.delete` | `cli_state_contract.rs:161` (row gone, second delete refused) |
| `cli.background.journal` | `cli_state_contract.rs:331` (fresh-home text and `[]` JSON) |
| `cli.schedule.resume` | `cli_state_contract.rs:219` (pause/resume, unknown id exit 1) |
| `cli.mcp` | `cli_state_contract.rs:283` (add/list/remove round trip) |
| `cli.mcp.add` | same test |
| `cli.mcp.list` | same test |
| `cli.mcp.remove` | same test |
| `cli.eval.trend` | `cli_state_contract.rs` (two real-shaped baselines, regression line, empty history refused) |

Executed 2026-11-22 on `main` at `153f0e19` with
`env -u NODE_OPTIONS cargo test -p leveler-cli --test cli_state_contract --test
cli_surface_contract`: 8/8 and 11/11 passed. The new tests are in the ordinary
test target; no assertion was weakened and no mock replaced a real process.

## Still open (35 of 50)

| Group | Count | Why open |
| --- | --- | --- |
| External account / provider / authorization | 6 | `cli.eval`, `cli.eval.ablate`, `cli.eval.compare` need a real model run and case set; `cli.login`, `cli.logout` need a provider credential flow; `cli.model.probe` needs provider network. A lab key would exercise the account, not the login contract |
| Installer / release channel | 2 | `install.homebrew`, `install.shell-script`: no local tap install or installer run; Phase C places the payload directly |
| TUI slash commands | 10 | `.attach`, `.clean`, `.context`, `.doctor`, `.export`, `.remote`, `.remote-loc`, `.skills`, `.theme`, `.update` need individual PTY contracts; Phase C does not exercise each |
| OS scheduler | 4 | `cli.schedule.enable/.disable/.uninstall/.doctor` touch live launchd/Task Scheduler; historical E2E is not re-executed here |
| Extended product | 13 | `cli.desktop-bridge`, `cli.execution-host`, `cli.remote.*` need a second device/host, live daemon or desktop host |

None is counted as passing. The 50-gap figure remains the published release's
condition until a later gate's inventory is re-mapped.

## Partial (4) and unverified (2)

| Feature | Status | Disposition |
| --- | --- | --- |
| `cli.background` | PARTIAL | soak driver exercises background success/failure; Phase E, not a CLI contract |
| `cli.run` | PARTIAL | exercised throughout Phases B/C/E; no single missing contract identified |
| `cli.serve` | PARTIAL | resident path exercised indirectly; no deterministic offline acceptance |
| `permission.windows-write-confine` | PARTIAL | needs a real Windows host |
| `tests.session-replay` | UNVERIFIED | corpus tests are `#[ignore]`d; not run |
| `tui.slash.fork` | UNVERIFIED | needs a PTY fork flow; not run |
