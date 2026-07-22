//! Configurable permission rules (SEC-1): tool / path / command_prefix → allow|ask|deny.
//!
//! Evaluated before profile `ApprovalPolicy`. Among matching rules:
//! **deny > ask > allow**. No match → fall through (`NoMatch`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::approval::{is_memory_write_tool, is_shell_wrapper_program};

/// Effect of a matching rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleEffect {
    Allow,
    Ask,
    Deny,
}

/// Match conditions (all set fields must match; empty match matches nothing useful).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleMatch {
    /// Exact tool name (e.g. `run_command`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Prefix of the rendered command line (`program` + args joined by space).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_prefix: Option<String>,
    /// Exact (whitespace-trimmed) command line. Used for compound shells that
    /// cannot be safely prefix-matched: only this one verbatim command is
    /// allowed, so an appended payload (`cmd; rm -rf /`) never rides the rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_exact: Option<String>,
    /// Glob matched against any path involved (simple `*` / `**` / `?`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_glob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    #[serde(rename = "match")]
    pub match_: RuleMatch,
    pub effect: RuleEffect,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRulesFile {
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

/// Ordered rule set (global first, project last so project can tighten).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionRuleSet {
    rules: Vec<PermissionRule>,
}

/// Result of evaluating rules against one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleDecision {
    Allow,
    Ask,
    Deny,
    NoMatch,
}

impl PermissionRuleSet {
    pub fn from_rules(rules: Vec<PermissionRule>) -> Self {
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// Merge: `other` rules appended (evaluated after self).
    pub fn extend(&mut self, other: PermissionRuleSet) {
        self.rules.extend(other.rules);
    }

    pub fn evaluate(
        &self,
        tool: &str,
        command_line: Option<&str>,
        paths: &[PathBuf],
    ) -> RuleDecision {
        let mut saw_allow = false;
        let mut saw_ask = false;
        let mut matched = false;
        for rule in &self.rules {
            if !rule_matches(&rule.match_, tool, command_line, paths) {
                continue;
            }
            matched = true;
            match rule.effect {
                RuleEffect::Deny => return RuleDecision::Deny,
                RuleEffect::Ask => saw_ask = true,
                RuleEffect::Allow => saw_allow = true,
            }
        }
        if !matched {
            return RuleDecision::NoMatch;
        }
        if saw_ask {
            RuleDecision::Ask
        } else if saw_allow {
            RuleDecision::Allow
        } else {
            RuleDecision::NoMatch
        }
    }
}

fn rule_matches(m: &RuleMatch, tool: &str, command_line: Option<&str>, paths: &[PathBuf]) -> bool {
    let mut any_constraint = false;
    if let Some(t) = &m.tool {
        any_constraint = true;
        if t != tool {
            return false;
        }
    }
    if let Some(prefix) = &m.command_prefix {
        any_constraint = true;
        let Some(cmd) = command_line else {
            return false;
        };
        if !cmd.starts_with(prefix.as_str()) {
            return false;
        }
    }
    if let Some(exact) = &m.command_exact {
        any_constraint = true;
        let Some(cmd) = command_line else {
            return false;
        };
        if cmd.trim() != exact.as_str() {
            return false;
        }
    }
    if let Some(glob) = &m.path_glob {
        any_constraint = true;
        if paths.is_empty() {
            return false;
        }
        if !paths.iter().any(|p| path_matches_glob(p, glob)) {
            return false;
        }
    }
    any_constraint
}

/// Minimal glob: `**` any path segment sequence, `*` within a segment, `?` one char.
fn path_matches_glob(path: &Path, pattern: &str) -> bool {
    let text = path.to_string_lossy().replace('\\', "/");
    let pat = pattern.replace('\\', "/");
    glob_match(&pat, &text)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    // Recursive backtracking for * and **.
    fn rec(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        if p.starts_with(b"**") {
            let rest = if p.len() > 2 && p[2] == b'/' {
                &p[3..]
            } else {
                &p[2..]
            };
            if rest.is_empty() {
                return true;
            }
            // Match rest at any slash-aligned position (or start).
            let mut i = 0;
            loop {
                if rec(rest, &t[i..]) {
                    return true;
                }
                if i >= t.len() {
                    return false;
                }
                // advance to next segment
                if let Some(rel) = t[i..].iter().position(|&c| c == b'/') {
                    i += rel + 1;
                } else {
                    i = t.len();
                }
            }
        }
        if p[0] == b'*' {
            // * within segment (no slash)
            let rest = &p[1..];
            let mut i = 0;
            loop {
                if rec(rest, &t[i..]) {
                    return true;
                }
                if i >= t.len() || t[i] == b'/' {
                    return false;
                }
                i += 1;
            }
        }
        if p[0] == b'?' {
            if t.is_empty() || t[0] == b'/' {
                return false;
            }
            return rec(&p[1..], &t[1..]);
        }
        if t.is_empty() || p[0] != t[0] {
            return false;
        }
        rec(&p[1..], &t[1..])
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

/// Load a rules file; missing file → empty set.
pub fn load_rules_file(path: &Path) -> Result<PermissionRuleSet, String> {
    if !path.is_file() {
        return Ok(PermissionRuleSet::default());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let file: PermissionRulesFile =
        serde_yaml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
    Ok(PermissionRuleSet::from_rules(file.rules))
}

/// Merge the three rule sources. Order is cosmetic — evaluation is
/// deny > ask > allow regardless:
/// - global: `<global_home>/permissions.yaml` (user-authored);
/// - project state: `state_rules` — the per-project file under the global
///   home (`Layout::permissions_path()`), where `ApproveAlways` persists
///   rules — never inside the repo;
/// - in-repo: `<repo>/.leveler/permissions.yaml` (user-authored / legacy
///   ApproveAlways target, still honored).
pub fn load_merged_rules(
    global_home: &Path,
    state_rules: &Path,
    repo_root: &Path,
) -> PermissionRuleSet {
    let mut set = load_rules_file(&global_home.join("permissions.yaml")).unwrap_or_default();
    set.extend(load_rules_file(state_rules).unwrap_or_default());
    set.extend(load_rules_file(&project_rules_path(repo_root)).unwrap_or_default());
    set
}

/// Path of the user-authored rules file under a repo root.
pub fn project_rules_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".leveler/permissions.yaml")
}

/// Derive durable allow rules for an `ApproveAlways` decision (SEC-1).
///
/// Intent: after the user picks **始终允许**, matching later calls (including
/// other files / similar commands in a plan) must stop re-prompting.
///
/// Granularity:
/// - `run_command` / simple `shell_command` → `program [first-arg]` prefix
///   (e.g. `git push`, `cargo test`);
/// - `apply_patch` / `replace` → **tool-level** allow (all paths for that tool),
///   so a plan can edit many files after one Always;
///
/// Returns empty when the call cannot be safely generalized (session-only):
/// - shell-wrapper programs (`sh -c …`): a prefix would grant every script;
/// - compound shell lines (`|`, `;`, `$…`) on `shell_command`;
/// - memory-write tools (K36);
/// - other tools with no safe standing shape.
pub fn always_rules_for(
    tool: &str,
    command: Option<&str>,
    _paths: &[String],
) -> Vec<PermissionRule> {
    if is_memory_write_tool(tool) {
        return Vec::new();
    }
    let allow = |match_: RuleMatch| PermissionRule {
        match_,
        effect: RuleEffect::Allow,
    };
    match tool {
        "run_command" | "shell_command" => command_rule(tool, command, allow),
        // Plan edits many files: one Always covers the whole edit tool.
        "apply_patch" | "replace" => vec![allow(RuleMatch {
            tool: Some(tool.to_string()),
            command_prefix: None,
            command_exact: None,
            path_glob: None,
        })],
        _ => Vec::new(),
    }
}

/// A standing allow rule for a shell-like tool. Simple commands get a
/// `program [first-arg]` prefix rule (covers `cargo test …` variants). A
/// compound shell (pipes, `$()`, `&&`, wrappers) cannot be prefix-generalized
/// safely, so it gets an EXACT rule instead of nothing — persisting the one
/// verbatim command the user approved without opening a hole for variants.
fn command_rule(
    tool: &str,
    command: Option<&str>,
    allow: impl Fn(RuleMatch) -> PermissionRule,
) -> Vec<PermissionRule> {
    if let Some(prefix) = durable_command_prefix(command) {
        return vec![allow(RuleMatch {
            tool: Some(tool.to_string()),
            command_prefix: Some(prefix),
            command_exact: None,
            path_glob: None,
        })];
    }
    let Some(exact) = command.map(str::trim).filter(|c| !c.is_empty()) else {
        return Vec::new();
    };
    vec![allow(RuleMatch {
        tool: Some(tool.to_string()),
        command_prefix: None,
        command_exact: Some(exact.to_string()),
        path_glob: None,
    })]
}

/// Prefix for a durable allow rule, or `None` when the command is unsafe to
/// generalize (wrappers, empty, shell metacharacters).
fn durable_command_prefix(command: Option<&str>) -> Option<String> {
    let cmd = command?.trim();
    if cmd.is_empty() {
        return None;
    }
    // Reject compound / interpolated shell so Always cannot become "any script".
    if cmd.chars().any(|c| {
        matches!(
            c,
            '|' | '&' | ';' | '<' | '>' | '$' | '`' | '\n' | '(' | ')' | '{' | '}'
        )
    }) {
        return None;
    }
    let mut tokens = cmd.split_whitespace();
    let program = tokens.next()?;
    if is_shell_wrapper_program(program) {
        return None;
    }
    Some(match tokens.next() {
        Some(second) => format!("{program} {second}"),
        None => program.to_string(),
    })
}

/// Append one rule to the project rules file (`<repo_root>/.leveler/
/// permissions.yaml`), creating `.leveler/` and the file when missing.
pub fn append_project_rule(repo_root: &Path, rule: &PermissionRule) -> Result<(), String> {
    append_rule_file(&project_rules_path(repo_root), rule)
}

/// Append one rule to a rules file, creating the parent directory and the
/// file when missing. A rule already present (identical match + effect) is a
/// no-op. An unreadable or unparseable existing file is an error naming the
/// file — it is never silently overwritten (an empty file counts as no
/// rules, matching the loaders' missing-file behavior).
pub fn append_rule_file(path: &Path, rule: &PermissionRule) -> Result<(), String> {
    let mut file = if path.is_file() {
        let raw =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if raw.trim().is_empty() {
            PermissionRulesFile::default()
        } else {
            serde_yaml::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?
        }
    } else {
        PermissionRulesFile::default()
    };
    if file.rules.iter().any(|existing| existing == rule) {
        return Ok(());
    }
    file.rules.push(rule.clone());
    let raw = serde_yaml::to_string(&file).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, raw).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Remove every project rule by deleting the project rules file (a missing
/// file is a no-op). Kept as a file delete rather than an empty `rules:`
/// list so the loaders' missing-file fast path stays the steady state.
pub fn clear_project_rules(repo_root: &Path) -> Result<(), String> {
    clear_rules_file(&project_rules_path(repo_root))
}

/// Remove one rules file; missing file is a no-op.
pub fn clear_rules_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(tool: &str, line: &str) -> RuleDecision {
        PermissionRuleSet::from_rules(vec![PermissionRule {
            match_: RuleMatch {
                tool: Some(tool.into()),
                command_prefix: Some(line.into()),
                command_exact: None,
                path_glob: None,
            },
            effect: RuleEffect::Allow,
        }])
        .evaluate(tool, Some(line), &[])
    }

    #[test]
    fn allow_by_command_prefix() {
        assert_eq!(cmd("run_command", "cargo test"), RuleDecision::Allow);
        let set = PermissionRuleSet::from_rules(vec![PermissionRule {
            match_: RuleMatch {
                tool: Some("run_command".into()),
                command_prefix: Some("cargo test".into()),
                command_exact: None,
                path_glob: None,
            },
            effect: RuleEffect::Allow,
        }]);
        assert_eq!(
            set.evaluate("run_command", Some("cargo test --workspace"), &[]),
            RuleDecision::Allow
        );
        assert_eq!(
            set.evaluate("run_command", Some("rm -rf /"), &[]),
            RuleDecision::NoMatch
        );
    }

    #[test]
    fn deny_beats_allow() {
        let set = PermissionRuleSet::from_rules(vec![
            PermissionRule {
                match_: RuleMatch {
                    tool: Some("run_command".into()),
                    command_prefix: Some("cargo".into()),
                    command_exact: None,
                    path_glob: None,
                },
                effect: RuleEffect::Allow,
            },
            PermissionRule {
                match_: RuleMatch {
                    tool: Some("run_command".into()),
                    command_prefix: Some("cargo clean".into()),
                    command_exact: None,
                    path_glob: None,
                },
                effect: RuleEffect::Deny,
            },
        ]);
        assert_eq!(
            set.evaluate("run_command", Some("cargo clean"), &[]),
            RuleDecision::Deny
        );
        assert_eq!(
            set.evaluate("run_command", Some("cargo test"), &[]),
            RuleDecision::Allow
        );
    }

    #[test]
    fn path_glob_star() {
        let set = PermissionRuleSet::from_rules(vec![PermissionRule {
            match_: RuleMatch {
                tool: Some("apply_patch".into()),
                command_prefix: None,
                command_exact: None,
                path_glob: Some("src/**".into()),
            },
            effect: RuleEffect::Ask,
        }]);
        assert_eq!(
            set.evaluate("apply_patch", None, &[PathBuf::from("src/lib.rs")]),
            RuleDecision::Ask
        );
        assert_eq!(
            set.evaluate("apply_patch", None, &[PathBuf::from("README.md")]),
            RuleDecision::NoMatch
        );
    }

    #[test]
    fn load_yaml_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.yaml");
        std::fs::write(
            &path,
            r#"
rules:
  - match: { tool: run_command, command_prefix: "echo " }
    effect: allow
"#,
        )
        .unwrap();
        let set = load_rules_file(&path).unwrap();
        assert_eq!(
            set.evaluate("run_command", Some("echo hi"), &[]),
            RuleDecision::Allow
        );
    }

    #[test]
    fn always_rules_run_command_prefix_is_program_first_arg() {
        let rules = always_rules_for("run_command", Some("cargo test --workspace"), &[]);
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].match_.command_prefix.as_deref(),
            Some("cargo test")
        );
        assert_eq!(rules[0].match_.tool.as_deref(), Some("run_command"));
        assert_eq!(rules[0].effect, RuleEffect::Allow);
        // The derived rule actually allows a matching later call.
        let set = PermissionRuleSet::from_rules(rules);
        assert_eq!(
            set.evaluate("run_command", Some("cargo test -p foo"), &[]),
            RuleDecision::Allow
        );
        assert_eq!(
            set.evaluate("run_command", Some("cargo clean"), &[]),
            RuleDecision::NoMatch
        );
        // No second token → just the program.
        let rules = always_rules_for("run_command", Some("ls"), &[]);
        assert_eq!(rules[0].match_.command_prefix.as_deref(), Some("ls"));
        assert!(always_rules_for("run_command", None, &[]).is_empty());
        assert!(always_rules_for("run_command", Some("  "), &[]).is_empty());
    }

    #[test]
    fn always_rules_shell_wrapper_is_exact_never_prefix() {
        // A `sh` prefix would grant every script, so wrappers/compound shells
        // must never get a prefix rule. They now get an EXACT rule instead of
        // nothing, so "Always" persists the one approved command verbatim.
        for cmd in [
            "sh -c 'cargo test'",
            "bash -lc 'ls'",
            "sh -c 'rm -rf x'",
            "cargo test | tee log",
        ] {
            let rules = always_rules_for("shell_command", Some(cmd), &[]);
            assert_eq!(rules.len(), 1, "{cmd}");
            assert_eq!(
                rules[0].match_.command_prefix, None,
                "no prefix rule: {cmd}"
            );
            assert_eq!(
                rules[0].match_.command_exact.as_deref(),
                Some(cmd),
                "exact rule for: {cmd}"
            );
        }
    }

    #[test]
    fn always_rules_simple_shell_command_gets_prefix() {
        let rules = always_rules_for("shell_command", Some("cargo test --workspace"), &[]);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_.tool.as_deref(), Some("shell_command"));
        assert_eq!(
            rules[0].match_.command_prefix.as_deref(),
            Some("cargo test")
        );
        let set = PermissionRuleSet::from_rules(rules);
        assert_eq!(
            set.evaluate("shell_command", Some("cargo test -p foo"), &[]),
            RuleDecision::Allow
        );
    }

    #[test]
    fn always_rules_shell_git_push_covers_later_similar_pushes() {
        // "始终允许" for shell `git push` must stop re-prompting for `git push origin main`.
        let rules = always_rules_for("shell_command", Some("git push"), &[]);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_.command_prefix.as_deref(), Some("git push"));
        let set = PermissionRuleSet::from_rules(rules);
        assert_eq!(
            set.evaluate("shell_command", Some("git push"), &[]),
            RuleDecision::Allow
        );
        assert_eq!(
            set.evaluate("shell_command", Some("git push origin main"), &[]),
            RuleDecision::Allow
        );
        assert_eq!(
            set.evaluate("shell_command", Some("git status"), &[]),
            RuleDecision::NoMatch
        );
    }

    #[test]
    fn always_rules_memory_writes_never_get_standing_permission() {
        for tool in ["remember", "forget", "consolidate_memory"] {
            assert!(
                always_rules_for(tool, None, &["notes.md".to_string()]).is_empty(),
                "K36: {tool} must not derive durable rules"
            );
        }
    }

    #[test]
    fn always_rules_apply_patch_is_tool_level_for_all_paths() {
        let paths = vec!["src/lib.rs".to_string(), "src/main.rs".to_string()];
        let rules = always_rules_for("apply_patch", None, &paths);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_.tool.as_deref(), Some("apply_patch"));
        assert!(rules[0].match_.path_glob.is_none());
        let set = PermissionRuleSet::from_rules(rules);
        // Later plan steps on other files must not re-prompt.
        assert_eq!(
            set.evaluate(
                "apply_patch",
                None,
                &[std::path::PathBuf::from("crates/other/src/x.rs")]
            ),
            RuleDecision::Allow
        );
        let rules = always_rules_for("replace", None, &[]);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].match_.tool.as_deref(), Some("replace"));
        assert!(always_rules_for("web_search", Some("x"), &[]).is_empty());
    }

    #[test]
    fn append_project_rule_creates_file_and_dir_then_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let rule = PermissionRule {
            match_: RuleMatch {
                tool: Some("run_command".into()),
                command_prefix: Some("cargo test".into()),
                command_exact: None,
                path_glob: None,
            },
            effect: RuleEffect::Allow,
        };
        append_project_rule(dir.path(), &rule).unwrap();
        let path = project_rules_path(dir.path());
        assert!(path.is_file());
        let set = load_rules_file(&path).unwrap();
        assert_eq!(set.rules(), std::slice::from_ref(&rule));
        // The written file keeps the `rules:` top level the loaders expect.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.starts_with("rules:"), "raw: {raw}");
        // Appending the identical rule is a no-op.
        append_project_rule(dir.path(), &rule).unwrap();
        assert_eq!(load_rules_file(&path).unwrap().rules().len(), 1);
        // A different rule appends.
        let second = PermissionRule {
            match_: RuleMatch {
                tool: Some("apply_patch".into()),
                command_prefix: None,
                command_exact: None,
                path_glob: Some("src/lib.rs".into()),
            },
            effect: RuleEffect::Allow,
        };
        append_project_rule(dir.path(), &second).unwrap();
        let set = load_rules_file(&path).unwrap();
        assert_eq!(set.rules(), &[rule, second]);
    }

    #[test]
    fn append_project_rule_errors_on_corrupt_yaml_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        let path = project_rules_path(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "rules: [not: valid").unwrap();
        let rule = PermissionRule {
            match_: RuleMatch {
                tool: Some("run_command".into()),
                command_prefix: Some("cargo test".into()),
                command_exact: None,
                path_glob: None,
            },
            effect: RuleEffect::Allow,
        };
        let err = append_project_rule(dir.path(), &rule).unwrap_err();
        assert!(err.contains("permissions.yaml"), "err: {err}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "rules: [not: valid"
        );
        // An empty file counts as no rules and is filled in.
        std::fs::write(&path, "").unwrap();
        append_project_rule(dir.path(), &rule).unwrap();
        assert_eq!(load_rules_file(&path).unwrap().rules().len(), 1);
    }

    /// ApproveAlways persists into the per-project state dir under the global
    /// home (`~/.leveler/projects/<hash>/permissions.yaml`); merged loading
    /// must read that file alongside the global and in-repo ones.
    #[test]
    fn merged_rules_include_the_state_dir_project_file() {
        let home = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let rule = PermissionRule {
            match_: RuleMatch {
                tool: Some("run_command".into()),
                command_prefix: Some("git push".into()),
                command_exact: None,
                path_glob: None,
            },
            effect: RuleEffect::Allow,
        };
        let state_rules = state.path().join("permissions.yaml");
        append_rule_file(&state_rules, &rule).unwrap();
        let set = load_merged_rules(home.path(), &state_rules, repo.path());
        assert_eq!(
            set.evaluate("run_command", Some("git push origin main"), &[]),
            RuleDecision::Allow
        );
    }

    #[test]
    fn clear_project_rules_removes_file_and_tolerates_missing() {
        let dir = tempfile::tempdir().unwrap();
        clear_project_rules(dir.path()).unwrap();
        let rule = PermissionRule {
            match_: RuleMatch {
                tool: Some("run_command".into()),
                command_prefix: Some("cargo test".into()),
                command_exact: None,
                path_glob: None,
            },
            effect: RuleEffect::Allow,
        };
        append_project_rule(dir.path(), &rule).unwrap();
        clear_project_rules(dir.path()).unwrap();
        assert!(!project_rules_path(dir.path()).exists());
        // Freshly merged rules no longer contain the cleared project rule.
        let set = load_merged_rules(dir.path(), &dir.path().join("state/permissions.yaml"), dir.path());
        assert!(set.is_empty());
    }

    #[test]
    fn always_rule_for_compound_shell_is_exact_not_prefix() {
        // A compound shell (command substitution, pipes, &&) cannot be made a
        // prefix rule — `$()` could hide anything, and a prefix is
        // append-exploitable (`cmd; rm -rf /` starts with `cmd`). "Always"
        // must still persist though, as an EXACT rule: only this one, verbatim
        // command is allowed, so it survives across sessions without opening a
        // hole for variants.
        let cmd = "TOKEN=$(curl -sf localhost/login) && echo \"$TOKEN\"";
        let rules = always_rules_for("shell_command", Some(cmd), &[]);
        assert_eq!(
            rules.len(),
            1,
            "compound shell must still get a durable rule"
        );
        assert_eq!(
            rules[0].match_.command_prefix, None,
            "compound shell must NOT get a prefix rule"
        );
        assert_eq!(
            rules[0].match_.command_exact.as_deref(),
            Some(cmd),
            "it must be an exact-match rule"
        );

        let set = PermissionRuleSet::from_rules(rules);
        assert_eq!(
            set.evaluate("shell_command", Some(cmd), &[]),
            RuleDecision::Allow,
            "the exact command is allowed across sessions"
        );
        // The whole safety point: an appended payload does NOT match.
        assert_eq!(
            set.evaluate("shell_command", Some(&format!("{cmd}; rm -rf /")), &[]),
            RuleDecision::NoMatch,
            "an appended payload must not ride the exact rule"
        );
        // A different command does not match either.
        assert_eq!(
            set.evaluate("shell_command", Some("echo hi"), &[]),
            RuleDecision::NoMatch
        );
        // Leading/trailing whitespace is normalized, not a bypass or a miss.
        assert_eq!(
            set.evaluate("shell_command", Some(&format!("  {cmd}  ")), &[]),
            RuleDecision::Allow
        );
    }
}
