# Foundation Daemon Crash-Consistency Closure

## 1. Status

```text
LOCAL_IMPLEMENTATION=COMPLETE
LOCAL_ACCEPTANCE=PASS
CI_ACCEPTANCE=PASS
DAEMON_CRASH_CONSISTENCY_CLOSURE=PASS
```

This gate starts from `667ef326373d4117e47d76dd584546a8f04158a4`
on `fix/daemon-crash-consistency`. It changes no database schema and no wire
schema.

## 2. Original Failure

The process-level test intermittently observed a durable `running` turn, killed
the daemon with SIGKILL, restarted it, and then found zero copies of the
initiating user message. The required count was one.

Thirty pre-change runs did not reproduce the timing window. That result is
recorded only as `NOT_REPRODUCED_IN_30_ROUNDS`; it is not evidence that the old
ordering was safe.

## 3. Canonical Data Ownership

For a fresh `user` or `chat` turn:

- `turns.payload` owns the versioned write-ahead initiating user message.
- `session_messages` owns the ordered transcript projection consumed by clients
  and later model requests.
- the Engine owns turn lifecycle and restart recovery;
- the Storage ports own transactional persistence and ownership fencing;
- the daemon transport owns the meaning of its wire ACK;
- `RuntimeEvent::UserMessageAdded` is an optimistic client notification. It is
  not a persistence command, a durability witness, or a canonical fact.

The payload and transcript are not competing authorities. The turn payload is
the recovery source for one accepted input; the transcript row is its ordered
projection. Recovery is keyed by turn identity, never by message content.

## 4. Persistence Sequence Before

```text
SubmitMessage
  -> ActiveTurns::admit                         (memory only)
  -> optional title/checkpoint work
  -> broadcast UserMessageAdded                (memory only)
  -> spawn background turn
  -> send() returns; daemon emits Ack

background:
  -> mark session running                      (SQLite commit)
  -> TurnStore::start_owned(payload=None)      (SQLite commit)
  -> persist TurnStarted                       (separate SQLite commit)
  -> executor calls TurnSink::append(user)     (later SQLite transaction)
```

The turn row and transcript append used independent transactions. Therefore a
SIGKILL could leave `turn=running`, `turn.payload=NULL`, and no user transcript
row. Neither the early UI notification nor the ACK made the input recoverable.

## 5. Persistence Sequence After

```text
SubmitMessage
  -> ActiveTurns::admit
  -> optional title/checkpoint work
  -> broadcast optimistic UserMessageAdded notification
  -> spawn background turn

background:
  -> mark session running
  -> TurnStore::start_owned(
       status=running,
       payload={version:1, initiating_message:<user message>}
     )                                          (one SQLite INSERT commit)
  -> persist TurnStarted
  -> release daemon ACK waiter
  -> executor appends the normal transcript row
  -> continue model/tool execution
```

On restart, every orphan running turn is examined before it is settled. If its
transcript has no user row for that `turn_id`, recovery appends the initiating
message under the current ownership token and then atomically records the
interrupted terminal event/projection.

## 6. Normative Contract

```text
DURABLE_RUNNING_TURN
    => DURABLE_AND_REPLAYABLE_INITIATING_INPUT

RECOVERY_COMPLETE
    => INITIATING_USER_MESSAGE_COUNT == 1
```

A fresh user/chat turn without an initiating user message is rejected by the
Engine before the turn row can be created. A resume turn carries no new input
and therefore has no initiating payload.

## 7. ACK Semantics

The daemon wire ACK now means:

```text
the fresh turn is durably running
AND
its canonical initiating input is durably replayable
```

It does not mean that model execution or the turn has completed. The daemon
composition root explicitly enables this boundary. Embedded in-process clients
retain their historical dispatch-only return and emit no transport ACK.

An error before durable admission is returned instead of ACKed. The
`TurnStarted` observer is safe as the waiter because `EventLog::append` commits
the event before forwarding it, and the turn row containing the input was
committed before that event.

## 8. Crash Window Matrix

| Actual window | Durable state at kill | Restart behavior | Client retry boundary |
| --- | --- | --- | --- |
| Before command receipt | none | nothing to recover | safe to send |
| After receipt, before turn INSERT | envelope receipt may be `dispatching`; no running turn and no ACK | no turn is invented | same command id is reported uncertain; caller must inspect |
| During turn INSERT | SQLite exposes neither half nor a partial row | nothing to recover if commit lost | no ACK |
| After turn INSERT, before transcript append | running turn plus versioned canonical input | project user message once, then interrupt turn | daemon ACK may be pending or delivered; accepted input is safe |
| After transcript append, before worker output | running turn plus input and transcript | detect existing user row by turn id; do not append again; interrupt turn | accepted input remains once |
| After worker starts, before terminal state | same, possibly with more transcript/events | recover canonical input if needed, then use normal crash recovery for the orphan turn/tool window | accepted input remains once |
| Handler returned, before wire ACK bytes | canonical input is durable | recovery is identical to post-ACK | an enveloped retry resolves through command receipt state |
| After wire ACK, before turn completion | canonical input is durable | recovery produces one transcript user message | raw `Send` has no cross-retry idempotency; production `Deliver` has `command_id` |
| Repeated restart | interrupted turn plus one projected user row | no running turn remains, so the next reap is a no-op | no recovery duplicate |

There is no separate “input committed but turn absent”, “turn created but not
running”, or “running without canonical input” window in the new design: those
facts are fields of the same inserted row and become visible in one SQLite
commit.

## 9. Root Cause

```text
ROOT_CAUSE=TURN_RUNNING_PRECEDED_INITIATING_INPUT_DURABILITY
PRODUCT_CRASH_CONSISTENCY_GAP=YES
RECOVERY_IDEMPOTENCY_GAP=YES_FOR_THE_MISSING_PROJECTION_PATH
TEST_WITNESS_INCORRECT=NO
EVENT_EMIT_IS_DURABILITY_BARRIER=NO_FOR_RuntimeEvent_UserMessageAdded
```

The existing test's running-row witness was valid for exposing the defect. The
product ordering, not its `marker_count == 1` assertion, was wrong.

## 10. Repair

- Require fresh `user` and `chat` turns to supply an initiating `Message` to
  `TurnRunner`.
- Serialize that message in a versioned turn payload before `start_owned`.
- Keep turn status and canonical input in one SQLite row/commit.
- Keep `UserMessageAdded` explicitly classified as an optimistic UI
  notification; daemon admission and recovery never rely on it.
- Make `serve` wait for durable turn admission before returning from command
  dispatch and therefore before emitting the wire ACK.
- Add `MessageStore::ensure_initiating_message_owned`, whose ownership check,
  turn-id existence check, ordinal allocation, and optional insert share one
  `BEGIN IMMEDIATE` transaction.
- Recover the transcript projection before settling an orphan running turn.

No retry was added. No assertion was weakened. No content-based deduplication
was added.

## 11. Mechanical Tests

The process-level deterministic test enables a unique barrier file beneath its
temporary `LEVELER_HOME`. The hook exists only behind the non-default
`test-crash-barrier` Cargo feature, is a no-op in default builds, and is neither
a CLI option nor a wire field.

`sigkill_after_durable_ack_before_transcript_append_recovers_once` proves:

1. `send` returned successfully, so the daemon ACK boundary was crossed;
2. the running row immediately contains the marker in its canonical payload;
3. the transcript still contains zero marker rows at the exact barrier;
4. SIGKILL runs without shutdown code;
5. first restart yields one marker row and no running turn;
6. second restart still yields one marker row.

`sigkill_during_a_task_recovers_on_restart_without_duplication` remains intact
and now also rejects an ACK that precedes the durable running/input row without
polling. The minimal non-Coding harness test constructs the same missing
projection state directly and proves the Engine repairs it.

## 12. Stress Evidence

```text
PRE_CHANGE_ROUNDS=30
PRE_CHANGE_FAILURES=0
PRE_CHANGE_CONCLUSION=NOT_REPRODUCED_IN_30_ROUNDS

POST_CHANGE_ROUNDS=30
POST_CHANGE_PASSED=30
POST_CHANGE_FAILED=0
MESSAGE_COUNT_ZERO=0
MESSAGE_COUNT_DUPLICATE=0
RECOVERY_DUPLICATE=0
```

The deterministic barrier is the correctness proof. The repeated E2E is the
stability sample.

## 13. CI Evidence

Final local acceptance on the reviewed task tree before remote CI:

```text
cargo fmt --all -- --check=PASS
cargo clippy --workspace --all-targets --all-features -- -D warnings=PASS
cargo test --workspace --all-features --locked --no-fail-fast=PASS
git diff --check=PASS
EXISTING_SIGKILL_E2E=PASS
DETERMINISTIC_ACK_CRASH_BARRIER_E2E=PASS
DEFAULT_BINARY_TEST_BARRIER=ABSENT
```

The commit identity is intentionally recorded only after the reviewed diff is
committed. Remote acceptance remains pending until the exact PR and main runs
complete on their first attempts.

```text
PRODUCT_COMMIT=f628c168f5c42ba6288a1a36387fa1c941e0dbc2
PR_NUMBER=18
PR_CI_RUN_ID=34710983155
PR_CI_ATTEMPT=1
PR_CI_FIRST_ATTEMPT_STABLE=YES
MAIN_CI_RUN_ID=34711668562
MAIN_CI_COMMIT=38e6a0304f8c38b65fe940853079e9fabadb3840
MAIN_CI_ATTEMPT=1
MAIN_CI_FIRST_ATTEMPT_STABLE=YES
```

Both runs completed successfully with all six jobs green: deny/audit, Web,
Mobile, and the Ubuntu, macOS, and Windows Rust matrices. No job was rerun.

## 14. Scope

Changed code is limited to interactive command admission, daemon composition,
Engine turn/reaper mechanics, the transcript persistence port and adapter,
focused tests, and the two architecture documents. There are no Browser,
`web_search`, Windows canary, provider, tool, reviewer, delegation, UI styling,
database schema, wire schema, or CI workflow changes.

## 15. Residuals

Exactly-once is guaranteed for automatic daemon recovery of the same accepted
turn. Production commands sent through `Deliver` also carry a durable
`command_id`. The raw `Send` seam has no client request id, so arbitrary client
resubmission after a lost response cannot be given cross-request exactly-once
semantics without a wire/API redesign; this gate does not claim otherwise.

The session status can become `running` before a turn is admitted. If the
process dies before the turn INSERT, no ACK was issued and no accepted input is
owed; this gate does not redefine session-status cleanup.

The canonical turn payload stores the complete initiating `Message`, including
base64 attachment data, until the turn is retained or removed. The normal
transcript stores the message again. This preserves replayability without a
schema change, but large attachments increase SQLite space and durable-ACK
latency. Moving media bodies behind durable content-addressed references is a
separate storage design change and is not hidden by this closure.

## 16. Final Gates

```text
DAEMON_CRASH_CONSISTENCY_ROOT_CAUSE_KNOWN=YES
CANONICAL_INITIATING_INPUT_IDENTIFIED=YES
CANONICAL_INPUT_DURABILITY_BOUNDARY_DEFINED=YES
DURABLE_RUNNING_TURN_IMPLIES_DURABLE_INPUT=YES
RECOVERY_COMPLETION_WITNESS_DEFINED=YES
RECOVERED_USER_MESSAGE_EXACTLY_ONCE=YES
AUTOMATIC_RECOVERY_IDEMPOTENT=YES
ACK_SEMANTICS_DEFINED=YES
ACK_DURABILITY_TESTED=YES
DETERMINISTIC_CRASH_BOUNDARY_TESTS=PASS
EXISTING_SIGKILL_E2E=PASS
THIRTY_ROUND_STRESS=PASS
NO_SLEEP_BASED_FIX=YES
NO_RETRY_MASKING=YES
NO_ASSERTION_WEAKENING=YES
ACTUAL_DIFF_REVIEWED=YES
INDEPENDENT_REVIEW=APPROVE
NO_SCOPE_EXPANSION=YES
DAEMON_CRASH_CONSISTENCY_CLOSURE=PASS
READY_FOR_WINDOWS_CANARY_SAMPLE_REVIEW=YES
READY_FOR_W3=NO
W3_STARTED=NO
FOUNDATION_FROZEN=NO
```

## 17. Handoff

The exact PR and main CI passed on their first attempts. The next gate is the
read-only Windows Canary Accumulated Evidence Review. W3-A starts only if that
review does not block.
