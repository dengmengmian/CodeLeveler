# HC-001 Freeze Record

Recorded before Run #1. Versions must not drift during the six runs.

## CodeLeveler

```
CODELEVELER_EVAL_BASELINE=3b400357342cef4caa760628531ead3bd9eff333
CODELEVELER_BINARY=eval/comparative/results/bin/leveler-3b400357
CODELEVELER_COMMAND=leveler run "<case task 原文>" --repo <case工作区> --model deepseek/deepseek-v4-flash --auto-approve
CODELEVELER_CONFIG=isolated LEVELER_HOME copy of ~/.leveler/config.toml (not edited)
CODELEVELER_MODEL=deepseek/deepseek-v4-flash
CODELEVELER_PROVIDER=deepseek
CODELEVELER_GATEWAY=https://taotoken.net/api/v1
CODELEVELER_SHA=3b400357342cef4caa760628531ead3bd9eff333
CODELEVELER_PERMISSION=assisted + --auto-approve
CODELEVELER_VERSION_DRIFT=NO
```

Binary identity (clean worktree build of the frozen SHA; not `~/.cargo/bin/leveler`, which was dirty):

```
leveler 0.2.0-beta.1 (3b400357342c)
sha256=d5189c81d781da9f61e9bb46ad6659db0f7211af93fbe3d8219114cdd40b7c72
```

Included relative to the previously documented re-freeze (`3269354f` + honesty closure):

| Precondition | SHA | In baseline? |
|---|---|---|
| Multi-Agent Restart Truth Hardening | `2c3863d4` | yes (ancestor) |
| TUI Agent Runtime Roster density/placement | `3269354f` | yes (ancestor) |
| wait_task bounded return | `36f2a547` | yes (ancestor) |
| icg-6r honesty closure (unsatisfiable → blocked) | `2b968534` | yes |
| icg-6r honesty closure (partial conflict → blocked) | `3b400357` | yes (HEAD) |

icg-6r **probe re-acceptance on this SHA has not been re-run**. Full Phase B matrix stays HOLD until the user reviews HC-001 (and, separately, until Phase A is re-run if they still want that gate). HC-001 itself is the six-run adapter/fairness probe, not the 60-run matrix.

Uncommitted workspace dirt (`.gitignore` / `Cargo.toml` version bump to beta.9 / eval docs) is **not** in the frozen binary.

## AtomCode

```
ATOMCODE_BINARY=~/.local/bin/atomcode
ATOMCODE_VERSION=5.0.9
ATOMCODE_SHA=52ca5e6
ATOMCODE_COMMAND=~/.local/bin/atomcode -p "<case task 原文>" -C <case工作区> -y -v --dev --no-telemetry
ATOMCODE_CONFIG=~/.atomcode/config.toml (DO NOT EDIT)
ATOMCODE_DEFAULT_PROVIDER=AtomGit-deepseek-v4-flash
ATOMCODE_ENDPOINT=https://llm-api.atomgit.com/v1
ATOMCODE_WIRE_MODEL=deepseek-v4-flash
ATOMCODE_CONFIG_SHA256=e8a928acb0137f82a1105f3345132242e524ab55993a5fbd66abd14eb5930109
ATOMCODE_VERSION_DRIFT=NO
```

`--dev` is mandatory because the real config has `auto_update = true`.

## DSH

```
DSH_EXECUTION_SOURCE=~/Develop/app/other/deepseek-harness
DSH_VERSION=0.1.2-alpha.1
DSH_SHA=cd5ef8148158c3a752a658978873241fdf8e2bbc
DSH_HOME_ISOLATED=YES
DSH_PERMISSION_MODE=danger-full-access
DSH_VERSION_DRIFT=NO
```

Invocation (cwd = case workspace):

```
node --import file://<DSH>/node_modules/tsx/dist/esm/index.mjs \
  <DSH>/apps/cli/src/bin.ts \
  --profile headless --patch <per-run patch.yml> \
  "<case task 原文>"
```

Old reference checkout `~/Develop/codeleveler-dogfood/reference/deepseek-harness` @ `99f6f02f` is **not** used.

## Model fairness (operational)

| Harness | Configured model | Endpoint | Wire `model` field |
|---|---|---|---|
| CodeLeveler | `deepseek/deepseek-v4-flash` | `https://taotoken.net/api/v1` | `deepseek-v4-flash` |
| AtomCode | `AtomGit-deepseek-v4-flash` (real default; not overridden) | `https://llm-api.atomgit.com/v1` | `deepseek-v4-flash` |
| DSH | patch `taotoken` / `deepseek-v4-flash` | `https://taotoken.net/api/v1` | `deepseek-v4-flash` |

```
MODEL_UPSTREAM_MATCH=PARTIAL
```

Same intended upstream model service (taotoken; AtomGit hop is user-confirmed, not packet-verified). Residual differences: AtomGit extra hop; AtomCode declared context window 512K vs CL 1M / DSH patch 1M; AtomCode `reasoning_effort_levels = high,max` vs CL `reasoning_effort = max`.

```
PERMISSION_FAIRNESS=ACCEPTABLE
```

All three runs unattended: CL `assisted + --auto-approve`; AtomCode `-y`; DSH `danger-full-access`. Semantic remainder recorded in `COMPARATIVE_FAIRNESS.md`.

## Case

See `docs/evaluations/HC-001-CONTRACT.md`.

```
TASK_TIMEOUT=1200
PROMPT_SHA256=497fd0fdf2db5698e2e78a34d28075b8023ea84607a7f66b74781b388e126633
BASELINE_RED=YES
```
