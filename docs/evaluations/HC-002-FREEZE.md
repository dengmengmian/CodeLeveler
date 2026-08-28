# HC-002 Freeze Record

Recorded before paid Run #1.

```
CODELEVELER_EVAL_BASELINE=7a263e931a4f3907c1a05d7407413d9e6a722924
CODELEVELER_RECONCILIATION_COMMIT=f759ff4a510a0e5ceabe87e19539cd38eaed3216
```

`7a263e93` is current `main` HEAD: reconciliation gate `f759ff4a` plus a remote-policy test-only follow-up (`7a263e93`, 24 lines in `remote_policy.rs`). No agent/runtime behavior change in the follow-up.

Clean binary (worktree build, not dirty `~/.cargo/bin/leveler`):

```
CODELEVELER_BINARY=eval/comparative/results/bin/leveler-7a263e93
CODELEVELER_COMMAND=leveler run "<task>" --repo <abs ws> --model deepseek/deepseek-v4-flash --auto-approve
CODELEVELER_IDENTITY=leveler 0.2.0-beta.1 (7a263e931a4f)
CODELEVELER_BIN_SHA256=51f6e9235b57a6d7ca722f57e395394c676ffe5cae1a87825e1ce88835818836
```

## Why not wait for icg-6r ×10 DONE

User authorized start after implementation commit. At freeze time the CC probe was still running; first four repetitions were all `Blocked` + `expect_passed=true` + no edits (honest). Residual risk: if later probe reps fail the 10/10 honesty line, this baseline may need replacement. That is recorded, not hidden.

## AtomCode / DSH (unchanged)

```
ATOMCODE_VERSION=5.0.9
ATOMCODE_SHA=52ca5e6
DSH_VERSION=0.1.2-alpha.1
DSH_SHA=cd5ef8148158c3a752a658978873241fdf8e2bbc
```

## Case

See `docs/evaluations/HC-002-CONTRACT.md`. `icg-5-long-task`, timeout 1800s.
