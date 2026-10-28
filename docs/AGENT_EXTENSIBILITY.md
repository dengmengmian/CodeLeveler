# Custom agents

[中文](AGENT_EXTENSIBILITY.zh-CN.md) · [Home](../README.md)

Custom agents provide reusable instructions for specific work, such as read-only code review or frontend development.

## Create an agent

Ask CodeLeveler to create one, or use Settings → Agents in the Web UI. Saving or deleting a definition through chat shows the proposed change and requires your confirmation.

To create one manually, make a directory with `agent.yaml` and `instructions.md`:

| Location | Scope |
| --- | --- |
| `.leveler/agents/<name>/` | Current project; share through Git |
| `~/.leveler/agents/<name>/` | All projects for this user |

Project definitions take precedence over user definitions, then built-ins. The names `default`, `explorer`, `worker`, and `reviewer` are reserved.

## Minimal example

Create `.leveler/agents/security-reviewer/agent.yaml`:

```yaml
version: 1
name: security-reviewer
description: Reviews code for security issues.
capability: read_only
```

Create `instructions.md` in the same directory:

```markdown
Review the changes the user specifies. Report only security issues with clear evidence.
For each issue, explain the impact, trigger, and fix. Say when no issues were found.
```

The name must match the directory, use lowercase letters, digits, and hyphens, and start with a letter. Do not put credentials in instructions; they are sent to the model.

## Common settings

| Field | Purpose |
| --- | --- |
| `capability` | `read_only` cannot write; `writer` writes within its declared scope; `scoped_writer` writes only assigned files |
| `model` | Optional configured `provider/model`; inherits the parent model when omitted |
| `reasoning_effort` | Optional reasoning level supported by the selected model |
| `skills` | Optional list of installed skill names |
| `tools` | Optional tool restrictions; cannot be empty or grant extra permissions |
| `workspace.write_roots` | Optional writable directories, relative to the repository; no `..` or wildcards |

For example, limit writing to the frontend directory:

```yaml
version: 1
name: frontend-worker
description: Implements frontend changes.
capability: writer
workspace:
  write_roots: [crates/leveler-web/web/src]
```

## Use and troubleshoot

Run `leveler agents list` to see available agents, `leveler agents show <name>` to inspect a definition, or `/agents` in the terminal.

Name an agent in your request, such as “Have security-reviewer review these changes.” Invalid definitions, missing models, and missing skills produce explicit errors; use `leveler doctor` to investigate. A missing API key can still fail at the actual model request.

Edits affect agents started afterward. Deleting a definition does not stop work already running.

Project agents come from the repository. Inspect their definitions before using an untrusted project. Instructions cannot expand tool or write permissions.
