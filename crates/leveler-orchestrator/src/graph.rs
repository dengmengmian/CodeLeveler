//! The task graph and per-node budget (spec §25, §27).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use leveler_core::{TaskId, TaskNodeId};

/// Structural problems that make a plan unsafe to execute or verify.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GraphValidationError {
    #[error("task graph is empty")]
    Empty,
    #[error("duplicate node id: {0}")]
    DuplicateId(String),
    #[error("node {node} depends on missing node {dep}")]
    MissingDependency { node: String, dep: String },
    #[error("dependency cycle involving: {ids}")]
    Cycle { ids: String },
}

/// The kind of work a node represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskNodeKind {
    Inspect,
    Design,
    Edit,
    Test,
    Verify,
    Review,
}

/// A node's lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Skipped,
}

/// A bounded budget for a single step/node (spec §27).
///
/// `max_commands` / `max_modified_files` / `max_duration` are enforced by the
/// executor (`0` = unlimited). `max_tool_rounds` is retired as a hard cap —
/// nodes run until terminal, bounded by the wall clock and the no-progress
/// guards — and is kept only for schema compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepBudget {
    pub max_tool_rounds: u32,
    pub max_modified_files: usize,
    pub max_commands: u32,
    pub max_repairs: u32,
    #[serde(with = "duration_secs")]
    pub max_duration: Duration,
}

impl Default for StepBudget {
    fn default() -> Self {
        Self {
            max_tool_rounds: 20,
            max_modified_files: 8,
            max_commands: 10,
            max_repairs: 2,
            max_duration: Duration::from_secs(15 * 60),
        }
    }
}

/// A single node in the task graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: TaskNodeId,
    pub kind: TaskNodeKind,
    pub description: String,
    #[serde(default)]
    pub dependencies: Vec<TaskNodeId>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub expected_outputs: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub budget: StepBudget,
    #[serde(default = "pending")]
    pub status: NodeStatus,
}

fn pending() -> NodeStatus {
    NodeStatus::Pending
}

/// An ordered set of task nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGraph {
    pub id: TaskId,
    pub goal: String,
    pub nodes: Vec<TaskNode>,
}

impl TaskGraph {
    /// Whether every required node reached a terminal-success/skip status.
    ///
    /// **Empty graphs are not "done"** for completion purposes — call
    /// [`validate`](Self::validate) first (`Empty` is an error). This method
    /// returns `true` for empty only vacuously; production paths must reject
    /// empty graphs before relying on it.
    pub fn all_done(&self) -> bool {
        !self.nodes.is_empty()
            && self
                .nodes
                .iter()
                .all(|n| matches!(n.status, NodeStatus::Completed | NodeStatus::Skipped))
    }

    /// Whether any node is still pending (or mid-run).
    pub fn has_unfinished(&self) -> bool {
        self.nodes
            .iter()
            .any(|n| matches!(n.status, NodeStatus::Pending | NodeStatus::Running))
    }

    /// Structural integrity: non-empty, unique ids, resolvable deps, acyclic.
    pub fn validate(&self) -> Result<(), GraphValidationError> {
        if self.nodes.is_empty() {
            return Err(GraphValidationError::Empty);
        }

        let mut seen: HashSet<&str> = HashSet::new();
        for n in &self.nodes {
            let id = n.id.as_str();
            if !seen.insert(id) {
                return Err(GraphValidationError::DuplicateId(id.to_string()));
            }
        }

        let ids: HashSet<&str> = self.nodes.iter().map(|n| n.id.as_str()).collect();
        for n in &self.nodes {
            for dep in &n.dependencies {
                if !ids.contains(dep.as_str()) {
                    return Err(GraphValidationError::MissingDependency {
                        node: n.id.to_string(),
                        dep: dep.to_string(),
                    });
                }
            }
        }

        // Kahn topological sort — leftover nodes form a cycle.
        let mut indegree: HashMap<&str, usize> = HashMap::new();
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for n in &self.nodes {
            indegree.entry(n.id.as_str()).or_insert(0);
            for dep in &n.dependencies {
                adj.entry(dep.as_str()).or_default().push(n.id.as_str());
                *indegree.entry(n.id.as_str()).or_insert(0) += 1;
            }
        }
        let mut q: VecDeque<&str> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut visited = 0usize;
        while let Some(id) = q.pop_front() {
            visited += 1;
            if let Some(children) = adj.get(id) {
                for child in children {
                    if let Some(d) = indegree.get_mut(child) {
                        *d = d.saturating_sub(1);
                        if *d == 0 {
                            q.push_back(child);
                        }
                    }
                }
            }
        }
        if visited != self.nodes.len() {
            let leftover: Vec<String> = indegree
                .iter()
                .filter(|(_, d)| **d > 0)
                .map(|(id, _)| (*id).to_string())
                .collect();
            return Err(GraphValidationError::Cycle {
                ids: leftover.join(","),
            });
        }
        Ok(())
    }

    /// The next pending node whose dependencies are all completed.
    pub fn next_ready(&self) -> Option<usize> {
        self.nodes.iter().position(|n| {
            n.status == NodeStatus::Pending
                && n.dependencies.iter().all(|dep| {
                    self.nodes
                        .iter()
                        .any(|m| &m.id == dep && m.status == NodeStatus::Completed)
                })
        })
    }
}

mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, deps: &[&str], status: NodeStatus) -> TaskNode {
        TaskNode {
            id: TaskNodeId::new(id),
            kind: TaskNodeKind::Edit,
            description: id.to_string(),
            dependencies: deps.iter().map(|d| TaskNodeId::new(*d)).collect(),
            allowed_paths: vec![],
            expected_outputs: vec![],
            acceptance_criteria: vec![],
            budget: StepBudget::default(),
            status,
        }
    }

    #[test]
    fn next_ready_respects_dependencies() {
        let graph = TaskGraph {
            id: TaskId::new("t"),
            goal: "g".into(),
            nodes: vec![
                node("a", &[], NodeStatus::Completed),
                node("b", &["a"], NodeStatus::Pending),
                node("c", &["d"], NodeStatus::Pending), // dep missing
            ],
        };
        assert_eq!(graph.next_ready(), Some(1));
    }

    #[test]
    fn all_done_detection() {
        let graph = TaskGraph {
            id: TaskId::new("t"),
            goal: "g".into(),
            nodes: vec![node("a", &[], NodeStatus::Completed)],
        };
        assert!(graph.all_done());
    }

    #[test]
    fn empty_graph_is_not_all_done_and_fails_validate() {
        let graph = TaskGraph {
            id: TaskId::new("t"),
            goal: "g".into(),
            nodes: vec![],
        };
        assert!(!graph.all_done());
        assert!(matches!(graph.validate(), Err(GraphValidationError::Empty)));
    }

    #[test]
    fn validate_rejects_duplicate_ids() {
        let graph = TaskGraph {
            id: TaskId::new("t"),
            goal: "g".into(),
            nodes: vec![
                node("a", &[], NodeStatus::Pending),
                node("a", &[], NodeStatus::Pending),
            ],
        };
        assert!(matches!(
            graph.validate(),
            Err(GraphValidationError::DuplicateId(_))
        ));
    }

    #[test]
    fn validate_rejects_missing_dependency() {
        let graph = TaskGraph {
            id: TaskId::new("t"),
            goal: "g".into(),
            nodes: vec![node("b", &["missing"], NodeStatus::Pending)],
        };
        assert!(matches!(
            graph.validate(),
            Err(GraphValidationError::MissingDependency { .. })
        ));
    }

    #[test]
    fn validate_rejects_cycle() {
        let graph = TaskGraph {
            id: TaskId::new("t"),
            goal: "g".into(),
            nodes: vec![
                node("a", &["b"], NodeStatus::Pending),
                node("b", &["a"], NodeStatus::Pending),
            ],
        };
        assert!(matches!(
            graph.validate(),
            Err(GraphValidationError::Cycle { .. })
        ));
    }

    #[test]
    fn validate_accepts_linear_dag() {
        let graph = TaskGraph {
            id: TaskId::new("t"),
            goal: "g".into(),
            nodes: vec![
                node("a", &[], NodeStatus::Pending),
                node("b", &["a"], NodeStatus::Pending),
            ],
        };
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn budget_duration_serializes_as_seconds() {
        let json = serde_json::to_value(StepBudget::default()).unwrap();
        assert_eq!(json["max_duration"], 900);
    }
}
