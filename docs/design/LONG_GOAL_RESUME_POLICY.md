# Long Goal — resume policy options

**Status:** proposal · 2026-08-26 · **Nothing implemented. No option chosen.**

Four options for what a runtime should do about a goal that still owes work,
evaluated against the evidence in
[LONG_GOAL_INTERRUPTION_FINDINGS](LONG_GOAL_INTERRUPTION_FINDINGS.md).

The recommendation is at the end and it is deliberately the smallest one.

## What the decision actually is

Not "should CodeLeveler support resume". It is:

> When a runtime finds work it owes, how much may it do without asking?

Every option below is a different answer to that, and the difference between
them is entirely about who carries the risk when the workspace is not what the
transcript says it is.

## The safety asymmetry

Resume is not symmetric with any other operation, because its failure mode is
silent.

A refused resume costs the user a click. A wrong resume produces confident work
on top of a broken workspace, recorded as if it were correct — and the agent
cannot tell, because "I did not get there" and "there was nothing there" look
identical from inside a truncated transcript.

That is not hypothetical. The Multi-Agent dogfood produced exactly it: a
reviewer cut off mid-review reported zero findings, and the UI rendered
"reviewed, nothing to flag". Four defects in this project have now had the same
shape — a missing measurement read as a measured zero.

**A resume that cannot distinguish those two states is that bug with a larger
blast radius.**

## Option A · Manual only

The runtime reports owed work. Continuing means the user starts a new goal, or
uses the existing `run --resume <id>`.

| | |
| --- | --- |
| Safety | Highest. The runtime never acts on its own |
| Workspace risk | None added |
| User expectation | Slightly under-serves: they see owed work and must re-issue it |
| Cost to build | **Zero. This is what P2 already ships.** |

## Option B · User-approved resume

The runtime offers to continue a specific goal after showing what it would be
continuing into: objective, workspace diff since the goal opened, where the
transcript stops, spend so far.

| | |
| --- | --- |
| Safety | High, *if* the evidence is shown. Low if it degrades to "Resume? [y/N]" |
| Workspace risk | Transferred to the user — legitimately, since they can look |
| User expectation | Matches: this is what a person expects a tool to offer |
| Cost to build | Moderate. Needs a workspace-diff view and a resume command |

The whole safety of this option lives in the evidence panel. A confirmation
prompt without those facts transfers responsibility without transferring
information, which is worse than not offering it.

## Option C · Safe automatic resume

Resume without asking when a set of conditions hold: the project builds, the
working tree matches the last recorded mutation, the transcript does not end
inside a tool call, and the goal was interrupted at a window boundary rather
than mid-tool.

| | |
| --- | --- |
| Safety | Depends entirely on the conditions being complete — and completeness is unprovable |
| Workspace risk | Carried by the runtime |
| User expectation | Delightful when right; alarming when wrong |
| Cost to build | High. Needs a workspace-consistency check that does not exist |

Worth noting: the case where these conditions hold is the *budget-stop* case,
and that one is **already handled** by the continuation supervisor without any
resume policy. So Option C's safe subset is largely work that is already done,
and its unsafe remainder is the part it cannot verify.

## Option D · Never resume certain states

Not an alternative — a constraint that applies to B and C. Some states must be
refused regardless of who asks:

| State | Why |
| --- | --- |
| Transcript ends inside an unresolved tool call | The next turn starts from a lie unless the gap is acknowledged (`acknowledge_crash_window` exists for this) |
| The task is owned by another runtime | The reaper already refuses to touch these; resume must too |
| Working tree changed outside the recorded mutations | Something else edited the repo; the transcript is no longer a description of reality |
| The goal's objective is stale beyond some age | Time passed, intent may not hold. Needs a stated threshold, not a guess |

## Comparison

| | A manual | B approved | C automatic |
| --- | --- | --- | --- |
| Silent wrong work possible | no | no | **yes** |
| Needs workspace-consistency check | no | for display | **as a gate** |
| Serves the unattended case | no | no | yes |
| Buildable today | shipped | yes | no |

The row that decides it is the first one.

## Recommendation

**Stay on A. Revisit when there is evidence of need.**

Not because B is wrong — B is probably where this lands — but because nothing
yet shows anyone wants it. The interruption findings are three scripted kills.
No user has come back to owed work and been unable to act on it.

The concrete proposal:

1. **Ship P2 as-is.** Owed work is visible, nothing acts on its own.
2. **Wait for the first real instance** of someone returning to an owed goal.
   That single case will say more about what resume should show than any amount
   of design.
3. **If B is then built, build the evidence panel first** and the resume
   command second. A resume button that ships before the panel will never grow
   one.
4. **Treat Option D as a hard constraint from the start,** not a later
   hardening pass. Refusing a dangerous resume is cheap; retrofitting the
   refusal after the happy path exists is not.

## What would change this recommendation

- Users repeatedly losing long-goal work → move to B sooner.
- Unattended/CI usage becoming primary → C's case strengthens, and so does the
  need for the consistency check it depends on.
- A workspace-consistency check arriving for another reason → C's main cost
  disappears and it deserves re-evaluation.

None of these is true today.

## Related

- [LONG_GOAL_INTERRUPTION_FINDINGS](LONG_GOAL_INTERRUPTION_FINDINGS.md)
- [LONG_GOAL_IDENTITY](LONG_GOAL_IDENTITY.md)
- [LONG_GOAL_CLOSURE_ANALYSIS](LONG_GOAL_CLOSURE_ANALYSIS.md)
