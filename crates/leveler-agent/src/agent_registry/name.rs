//! Agent names: the directory name and the identity the model and user use.

/// Longest agent name.
pub const MAX_AGENT_NAME_LEN: usize = 64;

/// Built-in names with structural runtime meaning. The runtime still has code
/// paths for these roles (the Worker's pre-claimed scope, the harness-launched
/// Reviewer), so a definition may not take the name and appear to change what
/// the role does.
pub const RESERVED_AGENT_NAMES: &[&str] = &["default", "explorer", "worker", "reviewer"];

/// Names Windows cannot create as a directory.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// `[a-z][a-z0-9-]*`, at most 64 characters, not ending in `-`.
///
/// Lowercase only, so `Foo` and `foo` cannot be two agents on a case-sensitive
/// filesystem and one on a case-insensitive one.
pub fn validate_agent_name(name: &str) -> Result<(), String> {
    let invalid = |why: &str| Err(format!("agent name `{name}` is invalid: {why}"));
    if name.is_empty() {
        return invalid("it is empty");
    }
    if name.len() > MAX_AGENT_NAME_LEN {
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

/// Whether `name` is a structural built-in that no definition may override.
pub fn is_reserved_agent_name(name: &str) -> bool {
    RESERVED_AGENT_NAMES.contains(&name)
}
