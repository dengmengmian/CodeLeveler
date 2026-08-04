//! Named sub-agents: reusable `spawn_agent` personas.
//!
//! Without these, every caller has to inline the whole persona into `task`, so
//! the same explorer/reviewer instructions get copy-pasted into each skill that
//! wants one. A named agent is defined once as Markdown with frontmatter and
//! referenced by name:
//!
//! ```markdown
//! ---
//! name: code-explorer
//! description: Traces execution paths through an existing feature.
//! role: explorer
//! ---
//! Your job is to map how <area> actually works …
//! ```
//!
//! Resolution order is project → user → built-in, so a repository can override
//! a shipped agent by dropping a file with the same name into
//! `.leveler/agents/`, exactly like skills.

use std::path::{Path, PathBuf};

/// Where a definition came from. Ordered by precedence (highest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    /// `<repo>/.leveler/agents/<name>.md`
    Project,
    /// `~/.leveler/agents/<name>.md`
    Global,
    /// Shipped with the binary.
    Builtin,
}

impl AgentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
            Self::Builtin => "builtin",
        }
    }
}

/// A reusable sub-agent persona.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedAgent {
    pub name: String,
    pub description: String,
    /// `explorer` / `worker` / `default`, as understood by
    /// [`crate::sub_agent::AgentRole`]. Empty means the caller decides.
    pub role: String,
    /// `provider/model` to run this agent on instead of the parent's model.
    /// Empty means inherit. Lets an investigation persona run on a cheap model
    /// while the parent stays on an expensive one.
    pub model: String,
    /// Tools this agent may use, by name. Empty = inherit the caller's set.
    /// A persona that only investigates has no business holding write tools.
    pub tools: Vec<String>,
    /// Max rounds this agent may run (0 = inherit). A focused persona that
    /// spins is worse than one that stops and reports.
    pub max_rounds: u32,
    /// The persona instructions prepended to the caller's `task`.
    pub instructions: String,
    pub source: AgentSource,
}

impl NamedAgent {
    /// The full instruction handed to the sub-agent: persona first, then the
    /// concrete assignment. A sub-agent starts a fresh conversation, so both
    /// halves have to travel together.
    pub fn compose_task(&self, task: &str) -> String {
        format!(
            "{}\n\n---\n\n## 本次任务\n\n{}",
            self.instructions.trim(),
            task.trim()
        )
    }
}

/// Agents shipped with the binary, as `(name, markdown)`.
const BUILTIN: &[(&str, &str)] = &[
    (
        "code-explorer",
        include_str!("../prompts/agents/code-explorer.md"),
    ),
    (
        "code-architect",
        include_str!("../prompts/agents/code-architect.md"),
    ),
    (
        "code-reviewer",
        include_str!("../prompts/agents/code-reviewer.md"),
    ),
];

pub fn project_agents_dir(root: &Path) -> PathBuf {
    root.join(".leveler").join("agents")
}

pub fn global_agents_dir() -> Option<PathBuf> {
    leveler_core::leveler_home_dir_from(|k| std::env::var_os(k)).map(|home| home.join("agents"))
}

/// Load one agent by name, project → global → built-in.
///
/// Returns `None` for an unknown name; callers must surface that rather than
/// silently spawning a personaless agent, or a typo turns into a subtly
/// different run that looks like it worked.
pub fn load(root: &Path, name: &str) -> Option<NamedAgent> {
    let name = name.trim();
    if name.is_empty() || !is_safe_name(name) {
        return None;
    }
    let file = format!("{name}.md");
    let mut candidates = vec![(project_agents_dir(root).join(&file), AgentSource::Project)];
    if let Some(global) = global_agents_dir() {
        candidates.push((global.join(&file), AgentSource::Global));
    }
    for (path, source) in candidates {
        if let Ok(content) = std::fs::read_to_string(&path) {
            return Some(parse(name, &content, source));
        }
    }
    BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, content)| parse(name, content, AgentSource::Builtin))
}

/// Every agent visible from `root`, deduplicated by name with project winning.
pub fn discover(root: &Path) -> Vec<NamedAgent> {
    let mut out: Vec<NamedAgent> = Vec::new();
    let mut dirs: Vec<(PathBuf, AgentSource)> =
        vec![(project_agents_dir(root), AgentSource::Project)];
    if let Some(global) = global_agents_dir() {
        dirs.push((global, AgentSource::Global));
    }
    for (dir, source) in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !is_safe_name(stem) || out.iter().any(|a| a.name == stem) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                out.push(parse(stem, &content, source));
            }
        }
    }
    for (name, content) in BUILTIN {
        if !out.iter().any(|a| a.name == *name) {
            out.push(parse(name, content, AgentSource::Builtin));
        }
    }
    out
}

/// Reject anything that could escape the agents directory or name a hidden
/// file; the name arrives from model-authored tool arguments.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn parse(fallback_name: &str, content: &str, source: AgentSource) -> NamedAgent {
    let normalized = content.replace("\r\n", "\n");
    let (front, body) = match normalized.strip_prefix("---\n") {
        Some(rest) => match rest.find("\n---") {
            Some(end) => (
                rest[..end].to_string(),
                rest[end + 4..].trim_start_matches(['\n', ' ']).to_string(),
            ),
            None => (String::new(), normalized.clone()),
        },
        None => (String::new(), normalized.clone()),
    };
    let field = |key: &str| -> Option<String> {
        front.lines().find_map(|line| {
            let (k, v) = line.split_once(':')?;
            (k.trim() == key).then(|| v.trim().trim_matches(['"', '\'']).to_string())
        })
    };
    NamedAgent {
        name: field("name")
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| fallback_name.to_string()),
        description: field("description").unwrap_or_default(),
        role: field("role").unwrap_or_default(),
        model: field("model").unwrap_or_default(),
        tools: field("tools")
            .map(|raw| {
                raw.split(',')
                    .map(|t| t.trim().trim_matches(['[', ']']).trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        max_rounds: field("max_rounds")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        instructions: body,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leveler-named-agent-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_agent(root: &Path, name: &str, content: &str) {
        let dir = project_agents_dir(root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.md")), content).unwrap();
    }

    /// The whole point: the three personas ship with the binary, so a fresh
    /// checkout can use them without anyone copying files around.
    #[test]
    fn the_shipped_agents_are_available_with_no_setup() {
        let root = tmp("builtin");
        for name in ["code-explorer", "code-architect", "code-reviewer"] {
            let agent = load(&root, name).unwrap_or_else(|| panic!("{name} must ship"));
            assert_eq!(agent.source, AgentSource::Builtin);
            assert!(
                agent.instructions.len() > 100,
                "{name} has no substantive persona"
            );
            assert!(!agent.description.is_empty(), "{name} needs a description");
        }
    }

    #[test]
    fn a_project_definition_overrides_the_shipped_one() {
        let root = tmp("override");
        write_agent(
            &root,
            "code-reviewer",
            "---\nname: code-reviewer\ndescription: 本仓库口径\nrole: explorer\n---\n只看并发安全。\n",
        );
        let agent = load(&root, "code-reviewer").unwrap();
        assert_eq!(agent.source, AgentSource::Project);
        assert_eq!(agent.instructions.trim(), "只看并发安全。");
    }

    /// A typo must not quietly become a personaless agent that looks like it
    /// worked — the caller has to be able to tell.
    #[test]
    fn an_unknown_name_is_not_resolved() {
        assert!(load(&tmp("unknown"), "code-explorerr").is_none());
    }

    /// The name comes from model-authored arguments; it must not be able to
    /// reach outside the agents directory.
    #[test]
    fn a_traversing_name_is_refused() {
        let root = tmp("traverse");
        assert!(load(&root, "../../etc/passwd").is_none());
        assert!(load(&root, "..").is_none());
        assert!(load(&root, ".hidden").is_none());
    }

    #[test]
    fn the_persona_travels_with_the_assignment() {
        let root = tmp("compose");
        write_agent(
            &root,
            "probe",
            "---\nname: probe\ndescription: d\nrole: explorer\n---\n你是探查者。\n",
        );
        let agent = load(&root, "probe").unwrap();
        let composed = agent.compose_task("查清登录流程");
        assert!(composed.contains("你是探查者。"));
        assert!(composed.contains("查清登录流程"));
        assert!(
            composed.find("你是探查者。") < composed.find("查清登录流程"),
            "persona must come first — a sub-agent reads top-down"
        );
    }

    #[test]
    fn the_definition_carries_its_own_role() {
        let root = tmp("role");
        write_agent(
            &root,
            "w",
            "---\nname: w\ndescription: d\nrole: worker\n---\nbody\n",
        );
        assert_eq!(load(&root, "w").unwrap().role, "worker");
    }

    /// The tool description advertises named agents; if the schema stops
    /// accepting the parameter the feature silently degrades to a personaless
    /// spawn that still "succeeds".
    #[test]
    fn the_spawn_tool_advertises_the_agent_parameter() {
        let def = crate::injected_tools::spawn_agent_tool_definition();
        let props = def.input_schema.get("properties").expect("properties");
        assert!(
            props.get("agent").is_some(),
            "spawn_agent must accept `agent`: {:?}",
            def.input_schema
        );
        assert!(
            def.description.contains("agent="),
            "the description must tell the model the parameter exists"
        );
    }

    /// The point of the override: an investigation persona can run on a cheap
    /// model while the parent stays on an expensive one.
    #[test]
    fn a_definition_may_pin_its_own_model() {
        let root = tmp("model");
        write_agent(
            &root,
            "cheap",
            "---\nname: cheap\ndescription: d\nrole: explorer\nmodel: deepseek/deepseek-v4-flash\n---\nbody\n",
        );
        assert_eq!(
            load(&root, "cheap").unwrap().model,
            "deepseek/deepseek-v4-flash"
        );
    }

    #[test]
    fn a_definition_without_a_model_inherits() {
        let root = tmp("nomodel");
        write_agent(
            &root,
            "plain",
            "---\nname: plain\ndescription: d\n---\nbody\n",
        );
        assert!(load(&root, "plain").unwrap().model.is_empty());
    }

    /// Policy as a field, not a caller-side ritual: a definition declares the
    /// tools it may hold and how long it may run, so every spawn of that agent
    /// is bounded the same way.
    #[test]
    fn a_definition_may_carry_its_own_policy() {
        let root = tmp("policy");
        write_agent(
            &root,
            "narrow",
            "---\nname: narrow\ndescription: d\nrole: explorer\ntools: [read_file, grep]\nmax_rounds: 8\n---\nbody\n",
        );
        let agent = load(&root, "narrow").unwrap();
        assert_eq!(agent.tools, vec!["read_file", "grep"]);
        assert_eq!(agent.max_rounds, 8);
    }

    #[test]
    fn a_definition_without_policy_inherits_everything() {
        let root = tmp("policy-none");
        write_agent(
            &root,
            "plain",
            "---\nname: plain\ndescription: d\n---\nbody\n",
        );
        let agent = load(&root, "plain").unwrap();
        assert!(
            agent.tools.is_empty(),
            "empty = inherit, not = none allowed"
        );
        assert_eq!(agent.max_rounds, 0);
    }

    #[test]
    fn discover_lists_builtins_and_project_agents_without_duplicates() {
        let root = tmp("discover");
        write_agent(
            &root,
            "code-explorer",
            "---\nname: code-explorer\ndescription: mine\n---\nbody\n",
        );
        write_agent(&root, "extra", "---\nname: extra\ndescription: d\n---\nb\n");
        let all = discover(&root);
        let explorers: Vec<_> = all.iter().filter(|a| a.name == "code-explorer").collect();
        assert_eq!(explorers.len(), 1, "override must not double-list");
        assert_eq!(explorers[0].source, AgentSource::Project);
        assert!(all.iter().any(|a| a.name == "extra"));
        assert!(all.iter().any(|a| a.name == "code-architect"));
    }
}
