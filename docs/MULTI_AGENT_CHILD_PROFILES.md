# Multi-agent child profiles

One resolve point: `ChildProfile::resolve(role)`. Capability is a semantic
class, not a role × tool-name matrix.

## Profiles

| Role | Registry | Scope | Blocking findings | Rounds | Tools |
| --- | --- | --- | --- | --- | --- |
| Explorer | `read_only_subset` (observe ∩ Safe) | none; `files` refused | no (flag stripped) | unbounded within parent residual | serial optional |
| Worker | full parent registry | required exclusive `files` | no | unbounded within residual | serial (`max_parallel_tools=1`) |
| Reviewer | same as explorer | none | yes | 20 | serial optional |
| Default | full parent registry | optional | no | unbounded | inherited |

Admission (`ChildProfile::admit`) is honest denial, never a silent downgrade:

- Worker without `files` → refused
- Explorer/Reviewer with `files` → refused

Same-batch overlapping Worker scopes (`scopes_overlap`: path or directory
prefix, not string prefix) refuse the second spawn.

## Mutation boundary

Explorer and Reviewer never receive mutating tools. A write call is an unknown
tool, not a runtime deny-after-the-fact. Worker writes are pinned by
`write_allowlist` before `apply_patch`/`replace` run, and by command write
constraints for shell.

Named personas may only narrow the registry, never add a write tool back.

## Negotiation (local, minimal)

Requested capability = role + optional file scope + named-agent narrowing.
Harness policy = profile + depth/concurrency/total caps.

This is not ACP, not a remote protocol, not a marketplace.

## What explorer cannot do

Browser tools are Network-risk (navigate) or not in the observe class. Explorer
investigates the workspace, not the live product UI. Browser read stays on
Main / Worker. Recorded as a known limitation, not a second permission system.
