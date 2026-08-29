//! Stamp build provenance into every binary that links the core crate.
//!
//! This lives in `leveler-core` rather than the CLI because the daemon
//! reports its own identity: a runtime that can only say `0.2.0-beta.1` is
//! indistinguishable from a differently-built runtime of the same version,
//! which is precisely how a stale daemon once served a session while the
//! binary on disk had already been replaced.
//!
//! Batch #1 shipped a release binary built from a checkout whose working tree
//! carried another session's uncommitted work: git HEAD said one thing, the
//! installed binary was something else, and only a hash comparison caught it.
//! A binary that cannot say where it came from makes every measurement taken
//! against it unfalsifiable, so provenance is compiled in rather than left to
//! the discipline of whoever ran the build.

use std::process::Command;

fn main() {
    // Rebuild when HEAD moves; a dirty tree cannot be watched this way, which
    // is exactly why the dirty flag is captured at build time.
    for path in [".git/HEAD", ".git/index"] {
        println!("cargo:rerun-if-changed=../../{path}");
    }

    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = match git(&["status", "--porcelain"]) {
        Some(out) => !out.trim().is_empty(),
        // No git available: say so rather than claim a clean tree.
        None => true,
    };
    println!("cargo:rustc-env=LEVELER_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=LEVELER_BUILD_DIRTY={dirty}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
