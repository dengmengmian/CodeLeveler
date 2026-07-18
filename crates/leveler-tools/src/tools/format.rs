//! Post-edit auto-formatting (spec §2.4 hook).
//!
//! After an edit tool writes a file, run the language's formatter on it so the
//! model never has to hand-fix indentation/style — which weak models otherwise
//! burn whole turns on. Best-effort and non-gating: a missing or failing
//! formatter never fails the edit, and formatters only rewrite valid code, so an
//! in-progress or unformattable file is left untouched.

use std::path::Path;
use std::time::Duration;

use leveler_execution::{PermissionProfile, ProcessRequest};
use tokio_util::sync::CancellationToken;

use crate::tool::ToolContext;

/// The per-file formatter command for a path, by extension. `None` means "no
/// formatter for this file type" (leave it alone).
fn format_command(path: &Path) -> Option<(&'static str, Vec<String>)> {
    let file = path.to_string_lossy().into_owned();
    match path.extension().and_then(|e| e.to_str()) {
        Some("go") => Some(("gofmt", vec!["-w".into(), file])),
        Some("rs") => Some(("rustfmt", vec![file])),
        Some("py") => Some(("ruff", vec!["format".into(), file])),
        _ => None,
    }
}

/// Format `resolved` in place after an edit, then re-fingerprint it. Silent on
/// every failure. Re-fingerprinting is essential: the formatter rewrites the
/// file the tool just recorded, so without it the next patch would see the
/// formatter's change as an outside edit and refuse.
pub(crate) async fn format_after_edit(context: &ToolContext, rel_path: &str, resolved: &Path) {
    if !context.auto_format {
        return;
    }
    let Some((program, args)) = format_command(resolved) else {
        return;
    };
    let mut req = ProcessRequest::new(program, args, context.workspace.root().to_path_buf());
    req.deny_network = true;
    req.timeout = Duration::from_secs(30);
    if context.mode == PermissionProfile::Assisted {
        req.write_root = Some(context.workspace.root().to_path_buf());
    }
    // Best-effort: never let a formatter error or absence fail the edit.
    let _ = context.runner.run(req, CancellationToken::new()).await;
    if let Ok(bytes) = tokio::fs::read(resolved).await {
        context.file_state.record(rel_path, &bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_formatter_by_extension() {
        assert_eq!(
            format_command(Path::new("/w/x.go")).map(|(p, _)| p),
            Some("gofmt")
        );
        assert_eq!(
            format_command(Path::new("/w/x.rs")).map(|(p, _)| p),
            Some("rustfmt")
        );
        assert_eq!(
            format_command(Path::new("/w/x.py")).map(|(p, _)| p),
            Some("ruff")
        );
        // gofmt must edit in place.
        let (_, args) = format_command(Path::new("/w/x.go")).unwrap();
        assert!(args.contains(&"-w".to_string()) && args.iter().any(|a| a.ends_with("x.go")));
    }

    #[test]
    fn no_formatter_for_unknown_types() {
        assert!(format_command(Path::new("/w/README.md")).is_none());
        assert!(format_command(Path::new("/w/data.json")).is_none());
        assert!(format_command(Path::new("/w/noext")).is_none());
    }

    /// End-to-end: a badly-indented Go file is reformatted after the hook runs.
    /// Skipped where `gofmt` is unavailable so CI without Go still passes.
    #[tokio::test]
    async fn gofmt_reformats_after_edit_when_available() {
        if std::process::Command::new("gofmt")
            .arg("-h")
            .output()
            .is_err()
        {
            return; // gofmt not installed — skip
        }
        let dir =
            std::env::temp_dir().join(format!("leveler-fmt-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        // Valid Go with ugly spacing; gofmt will normalize it.
        std::fs::write(
            dir.join("m.go"),
            "package m\nfunc  F( )  int  {\nreturn 1\n}\n",
        )
        .unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let environment = std::sync::Arc::new(leveler_core::EnvSnapshot::new(
            std::env::vars_os(),
            std::env::current_dir().unwrap_or_default(),
            std::env::temp_dir(),
        ));
        let ctx = ToolContext::with_environment(ws, PermissionProfile::Assisted, environment)
            .with_auto_format(true);
        let resolved = dir.join("m.go");

        format_after_edit(&ctx, "m.go", &resolved).await;

        let out = std::fs::read_to_string(&resolved).unwrap();
        assert!(
            out.contains("func F() int {"),
            "gofmt normalized it: {out:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
