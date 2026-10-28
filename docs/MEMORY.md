# Durable memory

[中文](MEMORY.zh-CN.md) · [Home](../README.md)

CodeLeveler can remember preferences, project conventions, and confirmed decisions for later tasks. Automatically collected inferences or conflicting information may need your confirmation.

## View and manage

Enter `/memory` in the terminal to search, read, copy, pin, or archive records. `/memory status` shows enabled features, pending records, and recent processing activity.

You can also ask CodeLeveler to remember a project's test command or forget a convention. A proposed write is not necessarily active; items requiring confirmation remain pending.

## Storage locations

| Scope | Location | Use |
| --- | --- | --- |
| Global | `~/.leveler/memory/` | Preferences shared across projects |
| Project | `.leveler/memory/` | Project conventions you can share through Git |
| Local | `.leveler/local/memory/` | Personal records for this project |

Records are Markdown files. Check project memory for private or personal information before committing it. Exclude local records through Git ignore rules as well.

## Settings

Configure memory in the project's `.leveler/config.yaml`:

```yaml
memory:
  enabled: true
  user_enabled: true
  experience_enabled: false
```

`enabled` controls all memory; `user_enabled` controls user facts. Experimental experience records require `experience_enabled`, which is off by default. Disabling a feature does not delete existing records.

## Privacy and limitations

Relevant memories may be sent to your configured model provider. Do not store passwords, tokens, or other credentials.

Records can be duplicated, outdated, or missed during retrieval. State important constraints in the current task and review stored records periodically.

Forgetting cannot remove copies in Git history, backups, or provider logs. Rotate any exposed credentials. Memory storage has only been verified on macOS and does not support network filesystems.
