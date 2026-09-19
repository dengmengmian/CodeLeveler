---
name: skill-creator
description: Create or improve a reusable Agent Skill from a procedure the user just worked out, or update an existing skill. Use when the user asks to "save this as a skill", "make this reusable", "turn this workflow into a skill", "$skill-creator", or to edit/refactor an existing skill. Covers whether a procedure deserves to be a skill, how to name it, how to write its trigger description, what belongs in SKILL.md vs scripts/ vs references/, project vs user scope, and how to avoid pinning temporary or secret context into a durable skill.
---

# Skill Creator

You are turning a procedure into a durable Agent Skill: a directory with a
`SKILL.md` (YAML frontmatter + Markdown body) and, when useful, bundled
`scripts/` and `references/`. A skill is loaded on demand and injected into
context when its name is mentioned (`$name`) or when its description clearly
matches the task. Write for a future agent that has none of this conversation.

## Decide whether it should be a skill

A skill is worth saving when the procedure is:

- **Reusable** — it will apply to more than this one task.
- **Non-obvious** — it encodes real steps, gotchas, or a working command
  sequence that a fresh agent would otherwise get wrong.
- **Stable** — the domain, not a single incident.

Do not create a skill for: a one-off fix, a fact already in the repo's
`AGENTS.md`/docs, a secret, a temporary path, or something the model already
does reliably without instructions. Say so and stop, rather than manufacturing
a skill.

If a skill of the same name already exists, prefer **update** over create, and
say which one you are changing and why in one sentence.

## Name

- Lowercase letters, digits and `-`; starts with a letter; ≤ 64 chars.
- Name the **capability**, not the incident: `windows-ci-debug`, not
  `fix-2431-jenkins-flake`.
- The directory name is canonical: the `name:` in frontmatter must equal it.

## Description (the trigger)

The description is the only text a future agent sees before deciding to load
the skill, so it must say **when** to use it, not just what it does. Include the
concrete trigger words a user would actually type ("intermittent CI failure",
"flaky test", "Windows CI"). Keep it one or two sentences; no newline.

## Write the body

- Lead with the procedure: ordered, concrete steps the agent can execute.
- Prefer exact commands, file paths relative to the skill directory, and
  decision points over prose. State the non-obvious failure modes.
- Put anything the agent must be able to **run** into `scripts/` (a shell,
  PowerShell or Python file), and reference it instead of pasting large code
  blocks into the body.
- Put long **reference material** (checklists, tables, API notes) into
  `references/`, and point at it from the body. Do not paste it inline.
- Keep the body about the procedure, never about this session: no "as we saw
  above", no temporary absolute paths, no tokens, keys, hostnames or personal
  data.

## Scope

- **project** (`.leveler/skills/`, shared through the repo): repo conventions,
  build/CI/deploy procedures for this codebase.
- **user** (`~/.leveler/skills/`, available in every project): personal
  workflow that is not repo-specific.

Choose from what the user said. If they did not say, ask — do not guess.

## Authoring rules

1. Propose first, never write directly. Call `save_skill`; the user confirms
   the exact proposal (name, description, scope, location, files) before
   anything is written.
2. Validate the name and description yourself before proposing, so a
   contradiction is never put to the user as a question.
3. Keep the description free of newlines and secrets.
4. `delete_skill` only removes project/user skills CodeLeveler manages. Built-in
   skills and skills discovered from other tools (`~/.codex/skills`,
   `~/.agents/skills`, `~/.claude/skills`) are readable and usable, but they are
   someone else's files: do not offer to delete or rewrite them.
5. When updating, send the complete new `SKILL.md` (and any bundled files you
   want kept) — the proposal replaces the skill directory.
