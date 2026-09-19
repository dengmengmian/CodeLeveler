//! Skill names: the directory name is the canonical identity.
//!
//! Two validators, because two different contracts meet here. A skill the user
//! authors through CodeLeveler is written to a fresh directory and mentioned as
//! `$name`, so it is held to a strict, portable shape. A skill another tool
//! installed is read where it already lives, so it only has to be a safe,
//! referenceable directory name — rejecting it for a cosmetic difference would
//! lose a package the user can already use.

/// Longest skill name.
pub const MAX_SKILL_NAME_LEN: usize = 64;

/// Names Windows cannot create as a directory.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// The strict shape CodeLeveler writes: `[a-z][a-z0-9-]*`, at most 64
/// characters, not ending in `-`, not a Windows device name.
///
/// Lowercase only, so the same name cannot be one skill on a case-sensitive
/// filesystem and another on a case-insensitive one.
pub fn validate_skill_name(name: &str) -> Result<(), String> {
    let invalid = |why: &str| Err(format!("skill name `{name}` is invalid: {why}"));
    if name.is_empty() {
        return invalid("it is empty");
    }
    if name.len() > MAX_SKILL_NAME_LEN {
        return invalid("it is longer than 64 characters");
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return invalid("it must start with a lowercase letter");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return invalid("use only lowercase letters, digits and `-`");
    }
    if name.ends_with('-') {
        return invalid("it must not end with `-`");
    }
    if WINDOWS_DEVICE_NAMES.contains(&name) {
        return invalid("it is a reserved device name on Windows");
    }
    Ok(())
}

/// The loose shape a discovered directory must have to be usable at all:
/// a single, non-hidden, referenceable path component that a `$name` mention
/// can be spelled from.
pub fn validate_read_name(name: &str) -> Result<(), String> {
    let invalid = |why: &str| Err(format!("skill directory `{name}` is unusable: {why}"));
    if name.is_empty() || name == "." || name == ".." {
        return invalid("it is not a usable name");
    }
    if name.len() > MAX_SKILL_NAME_LEN {
        return invalid("it is longer than 64 characters");
    }
    if name.starts_with('.') {
        return invalid("it starts with `.`");
    }
    if name.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return invalid("it contains whitespace or control characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return invalid("it contains characters that cannot be referenced");
    }
    Ok(())
}

/// Whether `$name` can name this skill. A name with characters outside the
/// mention alphabet (e.g. `.`) is still loadable by exact name, but not by
/// `$mention`; the index and `/skills` are the place that difference surfaces.
pub fn is_mentionable(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether `name` is a skill shipped with the binary; those cannot be edited or
/// deleted through the authoring store.
pub fn is_builtin_name(name: &str) -> bool {
    crate::builtin::is_builtin_name(name)
}
