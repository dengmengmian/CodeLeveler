//! `list_files` — list workspace entries up to a depth (spec §18.3).

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const IGNORED: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    "dist",
    "vendor",
    ".leveler",
];
const MAX_ENTRIES: usize = 2000;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Directory to list, relative to the workspace root. Defaults to ".".
    #[serde(default)]
    path: Option<String>,
    /// Maximum recursion depth. Defaults to 3.
    #[serde(default)]
    max_depth: Option<usize>,
}

pub struct ListFilesTool;

#[async_trait]
impl Tool for ListFilesTool {
    fn name(&self) -> &'static str {
        "list_files"
    }

    fn description(&self) -> &'static str {
        "List files and directories under a path relative to the workspace root \
         (or an absolute path under the workspace / a `--readonly-root`). \
         Up to a recursion depth; common build/vendor directories are skipped."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let rel = input.path.unwrap_or_else(|| ".".to_string());
        let max_depth = input.max_depth.unwrap_or(3);
        let base = context.workspace.resolve_read(&rel)?;

        if !base.is_dir() {
            return Ok(ToolOutput::error(crate::recoverable::path_not_directory(
                &rel,
            )));
        }

        let mut entries = Vec::new();
        walk(&base, &base, 0, max_depth, &mut entries);
        // Case-insensitive so `apple`, `Banana`, `Zebra` read in natural order
        // rather than ASCII order (all uppercase before all lowercase); ties
        // fall back to the exact ordering for determinism. Decorate once so the
        // comparator does not allocate lowercase copies O(n log n) times.
        let mut keyed: Vec<_> = entries
            .into_iter()
            .map(|entry| (entry.to_lowercase(), entry))
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        let mut entries: Vec<_> = keyed.into_iter().map(|(_, entry)| entry).collect();
        let truncated = entries.len() > MAX_ENTRIES;
        entries.truncate(MAX_ENTRIES);

        let mut out = entries.join("\n");
        out.push('\n');
        if truncated {
            // The walk stops early, so the real total is unknown.
            out.push_str(&format!(
                "… [listing capped at {MAX_ENTRIES} entries; pass a subdirectory \
                 `path` or a smaller `max_depth` to narrow]\n"
            ));
        }
        Ok(ToolOutput::ok(out))
    }
}

fn walk(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
) {
    if depth > max_depth || out.len() > MAX_ENTRIES {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let is_dir = path.is_dir();
        if let Ok(rel) = path.strip_prefix(root) {
            let mut display = rel.to_string_lossy().replace('\\', "/");
            if is_dir {
                display.push('/');
            }
            out.push(display);
        }
        if is_dir {
            walk(root, &path, depth + 1, max_depth, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_and_skips_ignored() {
        let dir = std::env::temp_dir().join(format!("leveler-ls-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = ListFilesTool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(out.content.contains("src/main.rs"));
        assert!(out.content.contains("Cargo.toml"));
        assert!(!out.content.contains("target"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn capped_listing_names_the_limit_and_recovery() {
        // The walk stops early, so the real total is unknown — the marker must
        // name the cap and tell the model how to narrow.
        let dir =
            std::env::temp_dir().join(format!("leveler-ls-cap-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(MAX_ENTRIES + 50) {
            std::fs::write(dir.join(format!("f{i:05}.txt")), "").unwrap();
        }
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = ListFilesTool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        assert!(
            out.content.contains(&format!("capped at {MAX_ENTRIES}")),
            "marker must name the cap: {}",
            &out.content[out.content.len().saturating_sub(200)..]
        );
        assert!(
            out.content.contains("path"),
            "marker must point at the narrowing knob: {}",
            &out.content[out.content.len().saturating_sub(200)..]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn entries_sort_case_insensitively() {
        let dir =
            std::env::temp_dir().join(format!("leveler-ls-sort-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["Zebra.txt", "apple.txt", "Banana.txt"] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = ListFilesTool
            .execute(serde_json::json!({}), ctx, CancellationToken::new())
            .await
            .unwrap();
        let order: Vec<&str> = out
            .content
            .lines()
            .filter(|l| l.ends_with(".txt"))
            .collect();
        assert_eq!(
            order,
            vec!["apple.txt", "Banana.txt", "Zebra.txt"],
            "listing must be case-insensitive, not ASCII (upper-before-lower)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn depth_limit_hides_deep_entries() {
        let dir =
            std::env::temp_dir().join(format!("leveler-ls-depth-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(dir.join("a/b/c")).unwrap();
        std::fs::write(dir.join("a/b/c/deep.rs"), "").unwrap();
        std::fs::write(dir.join("a/b/mid.rs"), "").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = ListFilesTool
            .execute(
                serde_json::json!({"max_depth": 2}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.content.contains("a/b/mid.rs"));
        assert!(!out.content.contains("a/b/c/deep.rs"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reports_not_a_directory() {
        let dir =
            std::env::temp_dir().join(format!("leveler-ls-nodir-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.rs"), "").unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = ListFilesTool
            .execute(
                serde_json::json!({"path": "file.rs"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("not a directory"));
        assert!(
            out.content.contains("read_file"),
            "recoverable copy should point at read_file: {}",
            out.content
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
