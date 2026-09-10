# Verification Semantics Closure

A repository operation was inheriting a source-change verification obligation.
`git pull` and `git switch` both succeeded and both finished as
`⚠ 已完成 · 验证未通过 · test`.

## 1. Problem

Two real dogfood turns:

- 「拉下最新代码」 — `git pull` fast-forwarded, exit 0, working tree clean.
- 「切换到 feat/oh0930 分支」 — `git switch` succeeded, working tree clean.

Both ended on the failed-checks marker naming `test`. The operation was right;
the verdict was wrong.

## 2. Root cause

The obligation is not chosen by task type. It is chosen by one question, asked
in `conclude_direct`:

```rust
// crates/leveler-engine/src/engine.rs
if outcome.modified_files.is_empty() || !spec.coding.verification.has_gates() {
    // nothing to verify
}
```

So everything turns on what counts as a modified file. That set is built from a
whole-tree diff taken around each command:

```rust
// crates/leveler-tools/src/tools/run_command.rs
match WorkspaceSnapshot::changed_since(&root, id).await {
    Ok(changed) => command_modified = changed,
    ...
}
```

`changed_since` is `git diff-tree` between the tree captured before the command
and the tree after it. **It cannot tell a file that was written from a file that
arrived because HEAD moved.** A fast-forward pull of 40 commits reports every
touched `.rs` file as modified; a branch switch reports every file that differs
between the branches.

From there the chain is deterministic:

| Step | What happens |
| --- | --- |
| `git pull` | tree diff reports N `.rs` paths |
| `AgentOutcome.modified_files` | N paths, none authored by the run |
| `conclude_direct` | non-empty → run the inferred plan |
| `VerificationPlan::for_languages([Rust])` | `cargo fmt` / `cargo check` / `cargo test --workspace` |
| `scope_gates_to_changes` | `.rs` files are build-relevant → gates stand |
| result | whole-workspace test verdict, attributed to a task that wrote nothing |

`scope_gates_to_changes` is the existing blast-radius escape hatch, and it
cannot help here: it only downgrades gates when *every* changed path is inert,
and pulled source is the opposite of inert.

So the defect is not "the runtime demands tests too often". It is that the
runtime's one input for "did this change need verifying" conflates two different
events.

## 3. The distinction that was missing

A command changes the working tree in two ways:

- **Authored mutation** — the run wrote a file. `apply_patch`, `replace`, a
  shell redirect, `sed -i`. What the file now says is this run's claim, and
  nothing but a build or a test can say whether the claim holds.
- **Repository transition** — the run moved `HEAD`. `pull`, `switch`,
  `checkout`, `reset`, `merge`. The content that appeared is content the
  repository already had, authored and (presumably) verified elsewhere. What the
  run is answerable for is *where HEAD ended up*, which git states directly.

Only the first carries a source-change obligation. The second's obligation is
discharged by the same command that performed it, at the moment it ran — no
extra command, no extra model round.

## 4. What changed

**`crates/leveler-execution/src/snapshot.rs`** — the position of HEAD becomes
observable, and a HEAD move becomes attributable:

- `WorkspaceSnapshot::head_commit` — the commit HEAD points at.
- `WorkspaceSnapshot::paths_explained_by_head_move(root, before, after)` — the
  paths that changed across the move **and** whose working-tree content is now
  exactly what the new HEAD records. A path the move touched but that is still
  dirty was written by whoever ran the command, so it is deliberately excluded.
- `WorkspaceSnapshot::head_moves_since_include_a_commit(root, before)` — whether
  the command made a commit. See below; anything the reflog cannot answer reads
  as "yes".

### The case tree state cannot decide

A command that writes a file and commits it in one invocation leaves *exactly*
the shape a pull leaves: HEAD moved, and the new content matches the new HEAD.
No comparison of trees can separate the two, and attributing the write away
would skip a gate that was genuinely owed — the false-verified outcome this
change exists to avoid creating.

Git's own reflog does record which it was, so that is what gets asked: the HEAD
moves made since the command started are walked, and if any of them created a
commit, nothing is attributed away. A reflog that is unreadable, disabled, or
too short to reach the starting commit reads the same way. Failing toward "the
command authored this" costs a verification run; failing the other way costs a
missed one.

**`crates/leveler-tools/src/tools/run_command.rs`** — `execute_program` (shared
by `run_command` and `shell_command`) captures the position before the command,
and when HEAD moved, subtracts the explained paths from what it *reports* as
modified.

The subtraction happens **after** the write-allowlist check, the file-budget
check and the rollback decision, and never touches the set those read. A
repository operation still cannot walk past a write scope. What changes is only
the answer to "what did this run author".

**Copy** — `REASON_NO_CODE_CHANGES` rendered as `◇ 结束 · 未改仓库`
("repo unchanged"), which is false after a pull. It now reads `◇ 结束 · 未改源码`
/ `◇ ended · no source edits`: a statement about what the run wrote, not about
where the repository stands.

## 5. Result

`git pull` / `git switch` now author nothing, so `conclude_direct` runs no
verification plan, so there is no `test` verdict to fail. The turn ends green.

Zero extra model rounds, in both directions: the runtime reads HEAD in the same
tool lifecycle that ran the command, and it never adds a round to decide whether
a verification is owed. The cost is a handful of `git` invocations, and only on
a command that actually moved HEAD.

## 6. What deliberately did not change

- **Code changes still owe verification.** `apply_patch` and `replace` report
  their paths as before. A command that moves HEAD *and* writes still reports
  the write — that is a test.
- **Permissions.** The scope, budget and rollback checks see every path the
  command touched, however it touched it.
- **Grounded authority (F7).** The runtime observes that HEAD moved. It does
  not claim to know which branch the user *meant*. `git switch feat/x` exiting 0
  onto a different branch is not something the runtime can call a goal failure
  without reading the user's intent, and it will not pretend to — git's own
  output is in the transcript, and the judgement stays with the reader.
- **Non-zero exit codes.** A command exiting 1 still marks that tool call
  failed. It never set the task's verdict — that came from the verification plan
  alone — so nothing here needs changing, and the runtime has no mechanical way
  to know that a given `grep` was used as a predicate rather than as a step.
  Guessing from command names would be worse than the honest per-call report.
- **`VerificationStatus`.** Still `Passed / Failed / NotRun / Unavailable`. A
  repository operation lands on `NotRun`, which is what it is: no check was
  owed, so none ran. No new variant was invented for a state the existing set
  already names.

## 7. Tests

| Test | Crate | Asserts |
| --- | --- | --- |
| `a_head_move_explains_only_the_paths_it_left_clean` | execution | the move explains the clean paths and only those |
| `a_head_move_is_not_an_authored_modification` | tools | `git switch` reports no modified files |
| `a_fast_forward_pull_authors_nothing` | tools | the reported case: `git pull --ff-only` reports none |
| `a_write_alongside_a_head_move_is_still_reported` | tools | a switch that also writes still reports the write |
| `a_write_committed_by_the_same_command_is_still_reported` | tools | writing and committing in one command still reports the write |
| `a_branch_switch_does_not_inherit_the_projects_test_gate` | engine | end to end: switch + a RED gate → `Completed` / `NotRun` |

Each was confirmed red before the fix. The engine test is the reported symptom
in one assertion — with the attribution removed it fails as:

```
a branch switch authors nothing: ["src/extra.rs", "src/lib.rs"]
```

and, with that assertion relaxed so the run reaches the verdict:

```
assertion `left == right` failed: a red source gate is not owed by a repository operation
  left: Failed
 right: NotRun
```

`Failed` is what the screenshot rendered as `⚠ 已完成 · 验证未通过 · test`.

## 8. Dogfood

A real model (`deepseek/deepseek-v4-flash`) over a scratch crate whose test
fails on purpose, cloned one commit behind its upstream, `--permission
full-access --auto-approve`:

| Turn | What the model did | Terminal line |
| --- | --- | --- |
| 拉下最新代码 | `git status`, then `git pull --ff-only origin main` (fast-forward, 2 files) | `Completed in 4 round(s); the project's checks did not run.` |
| 切换到 feat 分支，先把本地改动回退 | `git status` (clean, nothing to revert), then `git checkout feat` | `Completed in 4 round(s); the project's checks did not run.` |

Neither run printed a `Modified files` block, and neither claimed a failed
check. The workspace ended on `feat`, clean, holding that branch's files — the
operations really happened; only the false verdict is gone.

That line is `CompletedUnverified` carrying `REASON_NO_CODE_CHANGES`, which the
TUI renders as `◇ 结束 · 未改源码`.

The matching "before" was not re-run live — toggling the fix would have meant
rebuilding under a concurrent session's cargo jobs. It is pinned mechanically
instead, by the engine test above.

## 9. Known limits

- **Background commands.** `task_control`'s wait-end accounting
  (`account_background_mutations`) still uses the raw tree diff. A backgrounded
  `git pull` would still look authored. Left alone: backgrounding is for servers
  and watchers, and the reported case does not go through it.
- **Explicit user verification.** If a user asks "pull and run the tests", the
  runtime no longer imposes the test itself. The model runs it as a tool call
  and its result is in the transcript. The runtime does not read intent, so it
  cannot promote that request into its own gate.
- **Failed tool-run verifications are not in the ledger.** `drive` records a
  verification command only when it exits 0
  (`crates/leveler-agent/src/executor/drive.rs`), so a model-run `cargo test`
  that fails leaves no `VerifyRecord`. Pre-existing, unrelated to this defect,
  and it needs the exit code plumbed out of the dispatch result.
- **Repository operations under a narrow write scope.** A `git switch` in a task
  whose write allowlist is one crate still trips the allowlist and gets rolled
  back. Pre-existing; the rollback also does not move HEAD back.
