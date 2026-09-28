//! Factual model-facing descriptions of tool and permission failures.

pub fn missing_file(path: &str) -> String {
    format!("file not found: `{path}`.")
}

pub fn path_is_directory(path: &str) -> String {
    format!("`{path}` is a directory, not a file.")
}

pub fn path_not_directory(path: &str) -> String {
    format!("`{path}` is not a directory.")
}

pub fn sandbox_write_denied() -> &'static str {
    "\n[execution policy] Filesystem writes were confined to the granted paths.\n"
}

pub const NETWORK_PERMISSION_REQUIRED: &str = "[execution policy] Network access was denied";

pub fn network_permission_required() -> &'static str {
    "[execution policy] Network access was denied for this command.\n\n"
}

pub fn permission_refused(detail: &str) -> String {
    format!("[permission refused] {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_report_facts_without_prescribing_actions_or_causes() {
        assert_eq!(missing_file("nope.rs"), "file not found: `nope.rs`.");
        assert_eq!(
            path_is_directory("configs"),
            "`configs` is a directory, not a file."
        );
        assert_eq!(
            path_not_directory("file.rs"),
            "`file.rs` is not a directory."
        );
        for text in [sandbox_write_denied(), network_permission_required()] {
            assert!(
                !text.contains("retry")
                    && !text.contains("not a bug")
                    && !text.contains("did not contact"),
                "{text}"
            );
        }
        assert_eq!(
            permission_refused("network blocked"),
            "[permission refused] network blocked"
        );
    }
}
