# O1 / O2 Governance Resolution

```
O1_STATUS=NEVER_ESTABLISHED_AS_FROZEN_GATE
O2_STATUS=NEVER_ESTABLISHED_AS_FROZEN_GATE

O1_BETA_REQUIRED=NO
O2_BETA_REQUIRED=NO

BR_B=CLOSED
```

They were never Beta gates. Two things named `O`-something do exist in this
project, and neither is one — §3 says what they actually are, because "not a
gate" and "not a thing" are different claims and only the first is true.

---

## 1. Why this review exists

[`BETA_FINAL_PRODUCT_CLOSURE.md`](BETA_FINAL_PRODUCT_CLOSURE.md) audited every
Beta gate against its own recovered definition and could close all of them
except two. One was Runtime Version Consistency, whose own status line says its
dogfood never ran. The other was this:

> **BR-B · O1 and O2 have no recoverable definition.** Searched `docs/`,
> `evals/`, `ROADMAP.md`, `docs/design/`, git log and history. `O4-B` is
> recoverable. **`O1` and `O2` appear nowhere in this repository.**

A gate whose definition cannot be found cannot be marked `PASS`, and inventing
one to close it is the accounting this whole program exists to stop. So it was
recorded as `OPEN_REQUIRED` and left for this review.

The question is narrow: **were O1 and O2 ever frozen as Beta gates?**

## 2. Search scope

```
CURRENT_TREE_SEARCHED=YES
GIT_HISTORY_SEARCHED=YES
```

| Scope | Method |
| --- | --- |
| Current tree, all text types | `grep -rnoE '\bO1\b\|\bO2\b'` over `*.md *.rs *.toml *.yaml *.yml *.sh`, excluding `target/` |
| Full history, by content | `git log --all -S'O1'`, `-S'O2'`, `-S'O4-B'` |
| Full history, by message | `git log --all --grep='\bO[124]\b' -E` |
| Deleted / renamed documents | `git log --all --diff-filter=D --name-only` filtered for gate/matrix/roadmap/closure/beta |
| Companion series | `O4`, `O4-B` traced to their introducing commits |

## 3. What was found

**Exactly one substantive occurrence of `O1` exists in the entire history**,
and no standalone occurrence of `O2` at all.

### 3.1 `O1–O11` — an ownership eval matrix, not a gate

`eval/safety/manifest.yaml`, introduced by `999c692` *(feat(eval): add adoption
micro decision benchmark)*:

```yaml
ownership:
  case_rel: ownership-final/cases
  notes: Production matrix O1–O11. Do not fold into adoption denominators.
```

Three things settle what this is:

1. It is a **case index for the safety observer**, alongside sandbox and
   permission — `eval/safety/README.md` describes the whole directory as
   "Observer of ownership, sandbox, and permission. These are **not** adoption
   tasks."
2. The cases are **not in this repository**. The README points at
   `$CONTROL_ROOT/ownership-final/cases` — a private control plane — and names
   its case prefixes as `DJ, OV, PF, …`, not `O1, O2`.
3. Its own annotation excludes it from the metric it sits next to
   ("Do not fold into adoption denominators"), which is the behaviour of a
   measurement series, not of a release gate.

`O2` never appears on its own — only inside the range `O1–O11`.

### 3.2 `O4-B` — a finding id inside the F7 line

`O4-B` is real and recoverable, and it is a different namespace.
`F7_BEHAVIORAL_EVIDENCE_CHARACTERIZATION.md` §7:

```
            O4-B                     F7
   behaviour really happened,   judge asserts behaviour was
   proof cannot be bound        proved, on grounding that
              |                 cannot bear the claim
             CU                 False Verified
```

> O4-B is under-crediting real work; F7 is crediting work that did not happen.
> … This characterization does not fix O4.

It carries a semantics, it is cited across four F7 documents, and it is a
**finding**, not a frozen gate: no acceptance criterion, no evidence contract,
no closure document. "Does not fix O4" is a statement about scope, not a gate
status.

### 3.3 Nothing else

```
gate matrix documents deleted or renamed   : none
commit messages naming O1/O2/O4 as gates   : none
O1/O2 definition, acceptance criterion,
  evidence contract, closure document      : none
```

The `-S` hits on `O1`/`O2` in other commits (`f5517be`, `dd38df7`, `440e4e4`,
`032630f`, `5692e7a`, `6e26487`, `d15f32e`, `9a4f7eb`) were checked and are
incidental — base64 fragments, generated protocol text and build output, with
no gate context in any diff.

## 4. Where O1 / O2 entered the closure checklist

```
O1_O2_FIRST_ENTERED_CLOSURE_FROM=the Beta Final Product Closure handoff prompt
```

They arrived as labels in the closure workflow's own instruction, not from a
repository decision. Nothing in the repository ever pointed at them, and
`BETA_FINAL_PRODUCT_CLOSURE.md` is the first and only file in the project's
history to contain the token `O1` outside the ownership matrix line — it put it
there while recording that it could not be found.

```
GOVERNANCE_ROOT_CAUSE=
  undefined labels entered the closure checklist without repository-backed
  frozen gate definitions, most plausibly by confusion with the ownership
  production matrix O1–O11, which is an eval case series in an external
  control plane and was never a Beta gate
```

## 5. What this does **not** mean

It does not mean ownership went unaudited. The substance the `O1–O11` matrix
measures is covered by a gate that exists, is frozen, and passed:

| | |
| --- | --- |
| Gate | Multi-agent execution — `spawn_agent`, child lifecycle, ownership isolation, `claim_write_scope`, settlement, durable provenance |
| Evidence | FS1–FS16 PASS, 0 violations, **0 ownership denials** |
| Source | `BETA_RELEASE_READINESS.md` capability table; `MULTI_AGENT_PRODUCT_CLOSURE.md` |
| Status | `OPEN_BETA_BLOCKER = 0`, `OPEN_BETA_REQUIRED = 0` |

Nor does it mean O4-B is dismissed. It is open, it is measured, and it already
has a Beta disposition — the dogfood cohort observed it 11 times as
`CORRECT_AND_COMPLETED_UNVERIFIED`, and it is carried as
`AUTHORITY_YIELD_STATUS=CONCERN` /
`ACCEPT_AS_KNOWN_BETA_LIMITATION`. This review does not reopen it.

## 6. Disposition

```
O1_DEFINITION_FOUND=NO
O1_SOURCE=—
O1_STATUS=NEVER_ESTABLISHED_AS_FROZEN_GATE
O1_SUCCESSOR=—   (the same-named ownership matrix is an eval series,
                  not a superseding gate; ownership substance is covered
                  by Multi-agent execution, §5)

O2_DEFINITION_FOUND=NO
O2_SOURCE=—
O2_STATUS=NEVER_ESTABLISHED_AS_FROZEN_GATE
O2_SUCCESSOR=—

O4_REFERENCE_CONFIRMED=YES   (finding id in the F7 line, already dispositioned)
```

`SUPERSEDED` was considered and rejected. Calling the ownership matrix O1/O2's
successor would require explicit evidence of a rename or merge, and there is
none — only a shared letter. Mapping a gate onto a plausible-looking neighbour
is precisely the reasoning this program forbids.

A gate with no definition, no acceptance criterion and no evidence contract
cannot legitimately block a release. Removing it from the required ledger is
not a lowered standard; it is the repair of an invalid reference.

```
BR_B=CLOSED
```

## 7. Effect on Beta Final Product Closure

> **Point-in-time.** The block below is the state when this review closed BR-B,
> and BR-A was still open then. BR-A has since closed
> ([`RUNTIME_VERSION_CONSISTENCY_CLOSURE.md`](RUNTIME_VERSION_CONSISTENCY_CLOSURE.md)
> §5a), so `OPEN_REQUIRED` is now 0 and
> [`BETA_FINAL_PRODUCT_CLOSURE.md`](BETA_FINAL_PRODUCT_CLOSURE.md) is `PASS`.
> The body is left as it was written — an audit rewritten to agree with a later
> outcome stops being evidence.

```
OPEN_BLOCKERS=0
OPEN_REQUIRED=1        (was 2)

BR_A=OPEN              Runtime Version Consistency — non-idle drain proof
BR_B=CLOSED            this document

BETA_FINAL_PRODUCT_CLOSURE=HOLD
READY_FOR_BETA_BASELINE_FREEZE=NO

OPEN_BETA_BLOCKER=1
OPEN_BETA_REQUIRED=1
PHASE_B=HOLD
```

One required item remains, and it is a real one with a stated next step:
[`RUNTIME_VERSION_CONSISTENCY_CLOSURE.md`](RUNTIME_VERSION_CONSISTENCY_CLOSURE.md)
§7 — a PTY-driven task submission so the replacement is proven against a daemon
that is holding work, rather than an idle one.
