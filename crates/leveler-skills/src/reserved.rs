//! Built-in slash-command names reserved from the Skill namespace.
//!
//! A Skill named `help` is still a valid registry entry, but its slash identity
//! is `scope:help`; the bare `/help` always runs the product command. The list
//! is the contract between the client's command surface and the runtime's
//! leading-token parser. A TUI test asserts it matches `SLASH_DEFS` exactly.

/// Bare tokens (without the leading `/`) owned by product commands, including
/// aliases.
pub const RESERVED_SLASH_NAMES: &[&str] = &[
    "model",
    "permission",
    "permission-contract",
    "mode",
    "thinking",
    "goal",
    "btw",
    "recap",
    "collab",
    "agents",
    "skills",
    "review",
    "diff",
    "tasks",
    "find",
    "search",
    "jump",
    "trace",
    "context",
    "usage",
    "sessions",
    "rename",
    "restore",
    "checkpoint",
    "rewind",
    "fork",
    "compact",
    "recover",
    "copy",
    "output",
    "export",
    "save",
    "new",
    "resume-claude",
    "resume-codex",
    "attach",
    "image",
    "secret",
    "theme",
    "help",
    "exit",
    "quit",
    "doctor",
    "mcp",
    "settings",
    "clean",
    "trust",
    "schedules",
    "memory",
    "remember",
    "dream",
    "flush",
    "update",
    "upgrade",
    "reload",
];

/// Whether a bare slash token is owned by a product command.
pub fn is_reserved_slash_name(name: &str) -> bool {
    RESERVED_SLASH_NAMES.contains(&name.trim_start_matches('/'))
}
