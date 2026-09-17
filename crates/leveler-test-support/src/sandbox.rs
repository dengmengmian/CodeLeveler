//! Whether a test is itself running inside a verification sandbox.
//!
//! The product confines verification commands with `sandbox-exec` (macOS),
//! bubblewrap (Linux) or a Low-integrity label (Windows), and it points each
//! confined child's `LEVELER_HOME` at `<scratch>/leveler-home` under
//! `<home>/run/sandboxes/`. A test that then wants to *observe* confinement —
//! by running a confined child of its own — cannot: macOS refuses to apply a
//! second seatbelt profile inside the first (`sandbox_apply: Operation not
//! permitted`), and bwrap cannot nest either.
//!
//! Such a test must say so and stand down rather than assert. A failure there
//! would report the platform's nesting limit as a defect, and the worst place
//! for that is a self-dogfood run, where the suite under test is this
//! repository's own: "verify this repository with this repository's suite"
//! must not fail because the suite asked for a sandbox inside a sandbox.

use std::path::PathBuf;

/// True when this process is already inside a verification sandbox.
///
/// Recognizes the isolated root the sandbox hands its children, so the answer
/// comes from the same fact the product sets rather than from an env var a
/// test invents. Unconfined runs — CI included — answer `false`, which is
/// where the child-side guarantees are actually asserted.
pub fn already_confined() -> bool {
    let Some(home) = std::env::var_os("LEVELER_HOME").map(PathBuf::from) else {
        return false;
    };
    // The leaf name and the two path components are checked separately so the
    // answer does not depend on the host's separator.
    let components: Vec<String> = home
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    home.file_name().is_some_and(|name| name == "leveler-home")
        && components.iter().any(|c| c == "run")
        && components.iter().any(|c| c == "sandboxes")
}
