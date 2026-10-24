# Durable memory

中文：[`MEMORY.zh-CN.md`](MEMORY.zh-CN.md)

This is the single description of how CodeLeveler remembers things: where the
records live, who may change them, how they reach a model, and which limits are
known. It replaces the Memory 2.0 design, implementation and migration reports.
Layering and permission rules live in [`ARCHITECTURE.md`](ARCHITECTURE.md); this
document only covers the memory subsystem.

Status: Markdown memory is the only implementation. The JSON backend, the
`memory.backend` selector and the migration tooling were removed; a project
config that still carries `memory.backend` keeps loading and gets a notice, but
the value cannot select a store.

## 1. One authority, two derived caches

```text
user command ─┐
model tool ───┼─► MemoryService ─► Markdown current (authority)
auto extract ─┘         │
                        ├─► SQLite FTS5 index  (derived, rebuildable)
                        └─► catalog snapshot   (derived, cache-stable prefix)
```

| Fact | Authority | Lost or corrupted |
| --- | --- | --- |
| The record text, status, revision | Markdown `current` + private `history` | Unrecoverable for that revision |
| Retrieval candidates | SQLite FTS5 index | Rebuilt from Markdown |
| Injected catalog (id, revision, title) | `state/catalog/*.json` | Recomputed from Markdown |

Nothing reads a record from the index without revalidating it against Markdown:
a hit whose stored `revision`/`digest` differs from the file is dropped and its
index row removed. The index may suggest a candidate; only Markdown confirms one.

## 2. Scopes and paths

| Scope | Current records (editable, may be committed) | Private state |
| --- | --- | --- |
| Global | `~/.leveler/memory/*.md` | `~/.leveler/state/memory-v2/global/` |
| Project | `<repo>/.leveler/memory/*.md` | `~/.leveler/state/projects/<slug>/memory-v2/project/` |
| Local | `<repo>/.leveler/local/memory/*.md` | `~/.leveler/state/projects/<slug>/memory-v2/local/` |

`current` holds active records only. Everything else lives in the private state
root, one directory per concern:

```text
state_root/
  history/<id>/<revision>.md   every committed revision, including retired ones
  pending/<id>.md              candidates awaiting a person
  attic/<id>.md                snapshot of a retired record
  tombstones/<id>.json         what a forgotten record was (id only, no body)
  receipts/<id>.json           trust receipt written at the last trusted commit
  intents/<op>.json            op intent, for crash recovery
  journal/<op>.jsonl           commit journal, replayed on open
  catalog/<scope>.json         last injected catalog
  index/                       SQLite FTS5 database
  lock                         cross-process lock (shared read / exclusive write)
  suppressed.jsonl             rejected signals, so they are not re-proposed
  pending_history.jsonl        pending transitions
```

The project scope is workspace content: it is written into the repository and can
be reviewed, committed or edited by a person. The state root is refused when it
would land inside the repository or inside `.git`.

## 3. Record format

One file per record, YAML frontmatter plus a Markdown body:

```markdown
---
schema: 2
id: mem_<32 hex>
kind: user_fact | experience
scope: global | project | local
status: active | pending | deprecated | superseded | forgotten
revision: 3
authority: user_explicit | user_approved | user_stated | runtime_observed | model_inferred
title: 发布通道
subject: release.channel             # optional, human identity
subject_key: k-release-channel       # optional, canonical identity
value: blue                          # optional, short machine-readable value
statement: assertion | proposal
tags: []
pinned: false
pinned_by: user | model
sensitive: false
expires_at: 2026-12-01T00:00:00Z     # optional
supersedes: [mem_...]
superseded_by: mem_...
proposal: { operation, target_ids, expected_revision, reason }
experience: { standing, applies_when, invalid_if, evidence }
source_refs: []
created_at: 2026-10-24T00:00:00Z
updated_at: 2026-10-24T00:00:00Z
---

The body a person wrote.
```

| Field | Owner | Notes |
| --- | --- | --- |
| `id`, `kind`, `scope`, `revision`, `status`, `authority` | Runtime | Never accepted from a model or from frontmatter it wrote |
| `title`, `body` | Author of the revision | Empty titles and credential-shaped text are refused at the service boundary |
| `subject_key` | Writer, or derived from `subject` | Only an exact `subject_key` match is a shared identity |
| `pinned` / `pinned_by` | `pinned_by=user` is a standing constraint; a model pin is a ranking hint only | |
| `experience.*` | Runtime evidence | See §7 |
| `created_at`, `updated_at` | Runtime | |

Each committed revision gets a receipt in `state_root/receipts/` carrying
`id`, `scope`, `revision`, `digest`, `status`, `authority`, `pinned_by` and a
hashed `subject_key`. A current file that disagrees with its receipt is not
adopted: a changed body/title is `unverified`, a changed revision/status/subject
is a conflict. This is what stops a hand-edited file, an external editor or a
model-authored payload from promoting itself.

## 4. Authority

`authority` is what the runtime knows about who wrote the text, not what the text
claims.

| Writer | Resulting authority |
| --- | --- |
| Explicit user command (`/remember`, CLI, TUI edit) | `user_explicit` |
| User accepted a pending candidate | `user_approved` |
| User statement captured verbatim by automatic extraction | `user_stated` (only when proven, below) |
| Runtime observed a failure → resolution | `runtime_observed` |
| Model tool call or model inference | `model_inferred` |
| Imported or externally edited file | `ExternalUnverified`, capped at `model_inferred` |

Rules that follow from this table:

- **`user_stated` is proven, never claimed.** Automatic extraction reaches it
  only when the intent carries a trusted user-turn evidence span and the body is
  a verbatim, clause-bounded slice of that span: same letters and digits in the
  same order, with case and horizontal whitespace folded. Dropping a file path,
  a version, a number, a negation or a condition (`只在测试环境`) breaks
  contiguity or the clause boundary, so the candidate cannot be stored as the
  user's fact. It is parked instead, with authority `model_inferred`, and waits
  for a person. The rule is structural: no similarity score, no word table and
  no judge model participates.
- A model actor can never replace a user decision. It can only produce a
  pending candidate for a target the user already decided.
- `Full` permission mode does not create memory authority. Approving a tool call
  approves the action, not a durable fact about the user.
- `sensitive` records never become injectable and never enter the catalog.

## 5. Kinds and lanes

| Kind | Lane | What it holds | Default |
| --- | --- | --- | --- |
| `user_fact` | user | Preferences, decisions, corrections, notes the user stated | On |
| `experience` | experience | A lesson bound to observed evidence: failure → resolution → verification | Off |

`kind` is a closed set (`user_fact`, `experience`); `memory.kind` values such as
`preference`, `decision` and `note` are typed variants of a user fact, not
separate lanes.

Standing constraints are user facts only: `standing_rows` is restricted to
`kind = 'user_fact'` with `pinned_by = 'user'` or a user authority. An experience
record can never become a standing constraint, whatever its authority.

Project switches, in `.leveler/config.yaml`:

```yaml
memory:
  enabled: true            # false: no extraction, no recall, no memory tools
  user_enabled: true       # user facts
  experience_enabled: false # experimental lessons; off unless opted in
```

Disabling a lane stops its extraction and recall. It never deletes records.

## 6. Write paths

```text
/remember, CLI, TUI edit ─────► MemoryService (user command) ─► commit
model `remember` tool ────────► admit agent candidate ────────► pending | commit
turn-end extraction ──────────► candidate ──► admission ──────► pending | commit
failure → resolution ─────────► experience candidate ─────────► pending | commit
```

Automatic extraction runs after a user turn, out of band. It proposes candidates
with evidence spans; it does not write standing facts. The stored provenance
keeps both the span and the session/turn the runtime read, so a fact can always
be traced to a real message. A candidate that would replace a protected user
decision is always parked as pending, and a rejected signal is recorded in
`suppressed.jsonl` so it is not re-offered.

Admission decides `Applied`, `Pending` or `Skipped` from structured facts:
caller, target authority, statement kind, revision, subject identity, and — for
a `user_stated` fact — whether the body is a verbatim clause of the user-turn
span (`unverified_user_statement` when it is not). No synonym table, similarity
threshold or judge model participates.

## 7. Experience

An experience record carries `applies_when`, `invalid_if` and an `evidence` chain.
`standing = verified` needs both:

1. `evidence.to` proves a resolution was observed by the runtime, and
2. the current revision's authority is still `runtime_observed` or
   `user_approved`.

A user who rewrites the body with `user_explicit` demotes `standing` to
`observed`: the text is no longer the text the runtime verified. The evidence and
the older revisions stay readable for audit; re-verification means a new
observation, not an edit.

Experience extraction is off by default and must be opted into per project.

## 8. Pending and consent

Candidates live in the pending set of their own scope. The review surface lists
global, project and local pending together and accepts or rejects them
individually.

| Action | Effect |
| --- | --- |
| Accept | Commits the candidate with the accepting caller's authority |
| Reject | Removes the candidate and records the suppression |
| Headless run | Parks the candidate; it waits for a person, nothing is auto-accepted |

A run that cannot ask keeps working and leaves the candidate pending. There is no
path where a model, a headless run or a permission mode approves a durable fact
on the user's behalf.

## 9. Tools exposed to a model

| Tool | Surface |
| --- | --- |
| `memory` | `action`: `search`, `list`, `read`, `update`, `deprecate`, `pin`, `unpin`; `scope`: `project` (default), `local`, `global` |
| `remember` | Proposes one durable record: `title`, `body`, `subject`, `kind`, optional `value` |
| `forget` | Archives one record by id, and can restore it |

`update`, `deprecate` and `pin` require the `expected_revision` the caller read;
a mismatch is a conflict, not an overwrite. `read` is bounded per turn (call and
token budget) and reports `(truncated: showed bytes a..b of n; continue with
offset=b)` so a long body is read in pieces instead of being pasted whole.

A model write to a record the user decided is parked as pending, so a tool call
that returns "proposed" is not a completed memory change.

## 10. Retrieval and injection

One turn assembles the memory block from three sources:

| Segment | Contents | Position |
| --- | --- | --- |
| Catalog | `[scope][authority][id r<rev>] title` per active, non-sensitive user fact | Stable prefix |
| Standing constraints | User facts that are pinned or user-authored (max 8) | Advisory block |
| Recalled | FTS5 candidates per scope, revalidated against Markdown | Advisory block |
| Recalled experience | Lessons for the experience lane, minus conflicts with a user decision | Advisory block |

Per-scope limits: 12 user hits narrowed to 8 candidates, 6 experience hits
narrowed to 2. A recalled body is injected as data, never as an instruction, and
the block carries an omission count so the model can tell that something was
dropped.

When a `subject_key` is already occupied by a standing user fact or a candidate
user fact, the experience record for that key is not injected; it is counted as
an omission, stays stored and stays readable. Records with a different
`subject_key` keep their uncertainty. The resulting precedence is structural:

```text
current user message
  > standing user constraints
    > applicable candidate user facts
      > applicable experience not shadowed by a user decision
```

Every injection writes a receipt (`ids`, `catalog_ids`, `catalog_version`,
`catalog_digest`, block digest, omissions) and emits `MemoryRecalled`. The
receipt digest is computed over the injected bytes, so a report can prove which
request carried which memory.

## 11. Catalog invalidation

The catalog is part of the cache-stable prefix, so it may not be rewritten inside
a run. Each catalog snapshot carries a `version`; when current Markdown disagrees
with the injected snapshot, the runtime appends a tail delta for that run
(`added`, `updated`, `removed`, and never a title for a removed record) and
rebuilds the prefix on the next run, resume or compaction.

Only a user action can change the catalog, and a user cannot act inside a running
turn, so a live mid-turn delta is not reachable in normal use. The delta path is
covered by deterministic tests; it is not claimed as a live-verified behaviour.

## 12. Index

`state_root/index/` holds the derived FTS5 database:

- `documents` — one row per record: `id`, `scope`, `kind`, `status`, `revision`,
  `digest`, `title`, `subject`, `subject_key`, `value`, `tags`, `authority`,
  `pinned`, `updated_at`, `expires_at`, plus the four term columns.
- `documents_fts` — FTS5 over `title_terms`, `subject_terms`, `body_terms`,
  `tag_terms`, external content from `documents`, `unicode61` tokenizer,
  `bm25(3.0, 2.0, 1.0, 1.5)`.
- `meta` — `schema_version`, `tokenizer_version`, `dirty`.

Terms include CJK n-grams and identifier splitting, so a Chinese phrase and a
camel-case identifier both match. An incompatible schema or tokenizer version, a
failed `PRAGMA quick_check`, or an explicit rebuild deletes the index and marks it
dirty; the next read reconciles it from Markdown. Ranking is lexical only: there
is no vector recall, and a lexical hit never creates, replaces or merges a record.

## 13. Lifecycle operations

| Operation | Current | History | Index | Recoverable |
| --- | --- | --- | --- | --- |
| Update | New revision | Earlier revisions kept | Re-upserted | Yes |
| Deprecate | Retired to attic | Kept | Row removed | Yes, history is readable |
| Forget | Body deleted | Deleted | Row deleted | No; tombstone only |
| Pin / unpin | Same revision | — | Re-upserted | Yes |

Deprecate and forget are different operations and the TUI says so: `d` marks a
record as no longer true and keeps its history, `f` removes the managed body and
leaves a minimal tombstone. Rebuilding the index, restarting, resuming or
compacting must never resurrect a forgotten body.

A multi-file commit is not globally atomic across filesystems. The runtime keeps
an intent and a journal, replays them on open, and a reader holding the lock sees
either the state before or after the commit — not a half-applied one. A tool that
reads the directory directly can observe the transient window and must accept the
journal or check for it.

## 14. Surfaces

CLI:

```sh
leveler memory list|status|search|show|remember|pending|accept|reject|propose
leveler memory forget <id>        # deletes the managed body, keeps a tombstone
leveler memory diagnose          # inspect a scope without changing it
```

`/remember <text>` writes one record from the composer. `/memory` opens the page
at Home (active list, pending review, archived), `/memory review` opens the
pending queue, `/memory forget <id>` and `/memory accept|reject <id>` act by id.

TUI keys:

| Page | Keys |
| --- | --- |
| Home | `↑↓` select · `Enter` open · `Esc` back |
| Active / Archived list | `↑↓` move · `Enter` open · `/` filter · `s` scope · `t` kind · `x` clear filter |
| Pending review | `↑↓` move · `space` select · `Enter` open · `a` accept · `r` reject |
| Detail | `↑↓` scroll · `e` edit · `p` pin · `d` deprecate (press again to confirm) · `f` forget (press again to confirm) · `y`/`Y` copy body/id · `Esc` back |

Editing submits `title` and `body` only, with the revision the page opened. The
runtime keeps `id`, `kind`, `scope`, `authority`, `status`, `revision`, `pinned`,
`source_refs`, `experience` and `expires_at`.

## 15. Tests and dogfood

Deterministic tests own the contracts; a live run owns reachability.

```sh
cargo test -p leveler-memory          # store, schema, index, lifecycle, crash, concurrency
cargo test -p leveler-tools           # memory tool contract
cargo test -p leveler-app             # service wiring, extraction, injection
```

Scale benchmark (ignored by default):

```sh
MEMORY_SCALE=10000 MEMORY_EXPERIENCE=2000 \
  cargo test --release -p leveler-agent --lib memory_scale -- --ignored --nocapture
```

Live acceptance lives in the dogfood repository, not here:

```sh
cd ../dogfood
python3 eval/memory/live_acceptance.py --phase all \
  --binary /path/to/release/leveler --lab /tmp/memory-live
```

It runs a real `leveler tui` over a PTY against the configured provider, with an
isolated `LEVELER_HOME`, an isolated git workspace and a loopback capture proxy
that records the provider-visible request bytes while forwarding to the real
HTTPS endpoint. Every verdict reads the runtime's own facts: Markdown current,
the private state root, `sessions.db`, the request bytes. A provider that cannot
be reached is reported as blocked, never as a pass.

## 16. Performance

Measured on one machine, release build, 10k synthetic records, hot query matching
every record (the worst case for ranking):

| Configuration | p50 | p95 |
| --- | --- | --- |
| 10k, user lane only (shipped default) | 6.8–6.9ms | 7.4–10.1ms |
| 10k, both lanes, experience lane empty | 9.4–9.8ms | 10.3–12.4ms |
| 10k, both lanes, 2000 experience records | 10.8–10.9ms | 11.9–13.9ms |

Three consecutive runs per row. A single lexical pass over the 10k fixture costs
6.5–6.7ms at limit 12 and 6.0–6.2ms at limit 1, so nearly all of the user-lane
time is the ranked FTS5 scan, not candidate validation (48 batch reads: 2.4ms).
Index rebuild at 10k: 0.7–0.8s. Cold store open: ~150–350ms (journal recovery
plus orphan scan), warm catalog: sub-microsecond.

The user lane sits on the 10ms p95 target: two of three runs are under it and the
third is 10.1ms, so the margin is thin. The dual-lane rows are over it. The cost
is two lexical rankings of disjoint row sets: the lane filter already keeps each
query from scoring the other lane's rows, and FTS5 cannot apply `bm25()` in a
subquery, so a per-lane top-K is still two scans. Reducing it would mean merging
the two rankings (which cannot guarantee a per-lane K), shrinking K, or skipping
the Markdown revalidation — all of which trade recall or correctness for latency,
so none of them is implemented.

## 17. Known limits

1. **Paraphrase across subjects can duplicate.** Without embeddings or a judge,
   equivalence cannot be proven; only an exact `subject_key` or an explicit
   `supersedes` converges. An exact body match under a different subject is
   reported as `possible_duplicate`, never merged.
2. **Contradictions under different subjects coexist.** Both are injected as
   candidates and the model decides; the runtime does not pick a winner.
3. **Extraction is not proof.** `user_stated` needs a verbatim user clause, not
   a model summary of one: an extractor that rewrites loses the candidate to
   `unverified_user_statement` instead of gaining the user's authority. Verified
   experience needs a runtime receipt. A model that mislabels a proposal still
   only produces a pending candidate.
4. **Continuation turns are not extracted.** Automatic extraction is triggered by
   a fresh user turn; a correction typed mid-continuation reaches memory only
   through a direct command or a model tool.
5. **External edits cannot self-promote.** A receipt mismatch caps a file at
   `model_inferred`. Receipts do not defend against a process with the same
   privileges that can write the state root.
6. **FTS5 is lexical.** Chinese synonyms and cross-language paraphrases recall
   poorly by design; vectors are an explicit non-goal for this design.
7. **Forget is not cryptographic erasure.** Git objects, event logs, backups,
   provider logs and filesystem remnants can still hold the text. The tombstone
   only proves that the copies CodeLeveler manages are gone. A credential that
   reached a store must still be rotated.
8. **No cross-platform verification.** Rename, lock and fsync semantics are
   verified on macOS only; network filesystems are not supported for the state
   root.
9. **No absolute atomicity across filesystems.** Only journal recovery and reader
   isolation are guaranteed.

## 18. Invariants

- Markdown is the only authority; the index and catalog are rebuildable caches.
- One service decides every write and every lifecycle transition.
- A model cannot promote itself, and cannot overwrite a user decision.
- A permission mode never creates memory authority.
- `forget` removes the managed body; `deprecate` retires it and keeps history.
- Nothing resurrects a tombstoned body: not a rebuild, a resume, a compaction or
  a restart.
- The stable prefix only changes between runs; inside a run a change is a tail
  delta.
