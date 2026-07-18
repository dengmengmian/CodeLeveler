//! The requirement model (spec §24): the structured understanding of a task,
//! including verifiable acceptance criteria.

use serde::{Deserialize, Serialize};

/// The kind of engineering task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    BugFix,
    Feature,
    Refactor,
    Test,
    Docs,
    #[default]
    Other,
}

/// Coarse task risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRisk {
    Low,
    #[default]
    Medium,
    High,
}

/// A single, ideally-verifiable acceptance criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceCriterion {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub verification_hint: Option<String>,
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

/// The structured requirement derived from the raw task text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    #[serde(default)]
    pub raw_text: String,
    pub goal: String,
    #[serde(default)]
    pub task_type: TaskType,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    pub out_of_scope: Vec<String>,
    #[serde(default)]
    pub risk: TaskRisk,
    #[serde(default)]
    pub uncertainties: Vec<String>,
}

impl Requirement {
    /// A minimal fallback requirement when structured extraction fails.
    pub fn fallback(raw_text: &str) -> Self {
        Self {
            raw_text: raw_text.to_string(),
            goal: raw_text.to_string(),
            task_type: TaskType::Other,
            constraints: Vec::new(),
            // K11: fallback AC is optional so a weak-model understand failure
            // does not permanently block Verified when gates pass.
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "AC-1".to_string(),
                description: raw_text.to_string(),
                verification_hint: None,
                required: false,
            }],
            out_of_scope: Vec::new(),
            risk: TaskRisk::Medium,
            uncertainties: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_partial_requirement() {
        let json = serde_json::json!({
            "goal": "add cancel order",
            "acceptance_criteria": [
                {"id": "AC-1", "description": "paid orders cannot be cancelled"}
            ]
        });
        let req: Requirement = serde_json::from_value(json).unwrap();
        assert_eq!(req.goal, "add cancel order");
        assert_eq!(req.task_type, TaskType::Other);
        assert!(req.acceptance_criteria[0].required);
    }

    #[test]
    fn fallback_has_one_optional_criterion() {
        let req = Requirement::fallback("fix the bug");
        assert_eq!(req.acceptance_criteria.len(), 1);
        assert_eq!(req.goal, "fix the bug");
        let ac = &req.acceptance_criteria[0];
        assert_eq!(ac.id, "AC-1");
        assert!(!ac.required, "fallback AC must not block Verified (K11)");
        assert!(ac.verification_hint.is_none());
        assert_eq!(ac.description, "fix the bug");
    }
}
