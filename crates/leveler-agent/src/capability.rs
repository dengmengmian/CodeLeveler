//! Model-requested disclosure. Availability and permission are host facts;
//! active packs are scoped to one goal and never inferred from task text.
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use leveler_core::GoalId;
use leveler_core::OwnershipToken;
use leveler_model::ToolDefinition;
use leveler_storage::GoalStore;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityId {
    Browser,
    Memory,
    MultiAgent,
    Skills,
    CodeIntelligence,
    Web,
    Media,
    Authoring,
    HostInteraction,
    ExternalTools,
}
impl CapabilityId {
    pub const ALL: [Self; 10] = [
        Self::Browser,
        Self::Memory,
        Self::MultiAgent,
        Self::Skills,
        Self::CodeIntelligence,
        Self::Web,
        Self::Media,
        Self::Authoring,
        Self::HostInteraction,
        Self::ExternalTools,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Browser => "browser",
            Self::Memory => "memory",
            Self::MultiAgent => "multi_agent",
            Self::Skills => "skills",
            Self::CodeIntelligence => "code_intelligence",
            Self::Web => "web",
            Self::Media => "media",
            Self::Authoring => "authoring",
            Self::HostInteraction => "host_interaction",
            Self::ExternalTools => "external_tools",
        }
    }
    pub fn parse(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|id| id.as_str() == value)
            .ok_or_else(|| format!("unknown capability: {value}"))
    }
    fn description(self) -> &'static str {
        match self {
            Self::Browser => "Browse/interact with pages",
            Self::Memory => "Recall/persist context",
            Self::MultiAgent => "Delegate work",
            Self::Skills => "Load instructions",
            Self::CodeIntelligence => "Symbols/references/diagnostics",
            Self::Web => "Fetch/search web",
            Self::Media => "Inspect images",
            Self::Authoring => "Manage agents/skills",
            Self::HostInteraction => "Ask the user",
            Self::ExternalTools => "Connected MCP tools",
        }
    }
}

pub fn tool_capability(name: &str) -> Option<CapabilityId> {
    match name {
        name if name.starts_with("browser_") => Some(CapabilityId::Browser),
        "memory" | "remember" | "forget" => Some(CapabilityId::Memory),
        "spawn_agent" => Some(CapabilityId::MultiAgent),
        "load_skill" => Some(CapabilityId::Skills),
        "find_symbol" | "read_symbol" | "find_references" | "diagnostics" | "blast_radius" => {
            Some(CapabilityId::CodeIntelligence)
        }
        "web_fetch" | "web_search" => Some(CapabilityId::Web),
        "view_image" => Some(CapabilityId::Media),
        "list_agents" | "save_agent" | "delete_agent" | "save_skill" | "delete_skill" => {
            Some(CapabilityId::Authoring)
        }
        "request_user_input" | "ask_user" => Some(CapabilityId::HostInteraction),
        name if name.starts_with("mcp__") => Some(CapabilityId::ExternalTools),
        _ => None,
    }
}

#[derive(Clone)]
struct GoalBinding {
    store: Arc<dyn GoalStore>,
    token: OwnershipToken,
    goal: GoalId,
}

pub struct CapabilityDisclosure {
    available: BTreeSet<CapabilityId>,
    permitted: BTreeSet<CapabilityId>,
    active: Arc<RwLock<BTreeSet<CapabilityId>>>,
    binding: RwLock<Option<GoalBinding>>,
    enable_gate: Arc<tokio::sync::Mutex<()>>,
}
impl CapabilityDisclosure {
    pub fn new(
        available: Vec<CapabilityId>,
        permitted: Vec<CapabilityId>,
        active: Vec<CapabilityId>,
    ) -> Result<Self, String> {
        let available: BTreeSet<_> = available.into_iter().collect();
        let permitted: BTreeSet<_> = permitted.into_iter().collect();
        let active: BTreeSet<_> = active.into_iter().collect();
        if !active.is_subset(&available) || !active.is_subset(&permitted) {
            return Err("restored capability unavailable or not permitted".into());
        }
        Ok(Self {
            available,
            permitted,
            active: Arc::new(RwLock::new(active)),
            binding: RwLock::new(None),
            enable_gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
    pub fn available(&self, id: CapabilityId) -> bool {
        self.available.contains(&id)
    }

    pub async fn enable_with_authority(
        &self,
        id: CapabilityId,
        unrestricted: bool,
    ) -> Result<bool, String> {
        if unrestricted {
            if !self.available(id) {
                return Err(format!("capability unavailable: {}", id.as_str()));
            }
            // Full already exposes every mechanically available capability;
            // bypassing permission must not manufacture a persisted grant.
            return Ok(false);
        }
        self.enable(id).await
    }

    pub fn active(&self) -> Vec<CapabilityId> {
        self.active
            .read()
            .expect("capability lock")
            .iter()
            .copied()
            .filter(|id| self.available.contains(id) && self.permitted.contains(id))
            .collect()
    }
    pub fn exposed(&self, id: CapabilityId) -> bool {
        self.available.contains(&id)
            && self.permitted.contains(&id)
            && self.active.read().expect("capability lock").contains(&id)
    }
    pub fn permits_tool(&self, name: &str) -> bool {
        tool_capability(name).is_none_or(|id| self.exposed(id))
    }
    pub fn for_child(&self, tools: &[ToolDefinition]) -> Self {
        let offered: BTreeSet<_> = tools
            .iter()
            .filter_map(|tool| tool_capability(&tool.name))
            .filter(|id| {
                !matches!(
                    id,
                    CapabilityId::MultiAgent
                        | CapabilityId::Authoring
                        | CapabilityId::HostInteraction
                        | CapabilityId::ExternalTools
                )
            })
            .collect();
        Self {
            available: self.available.intersection(&offered).copied().collect(),
            permitted: self.permitted.intersection(&offered).copied().collect(),
            active: self.active.clone(),
            binding: RwLock::new(self.binding.read().expect("capability lock").clone()),
            enable_gate: self.enable_gate.clone(),
        }
    }
    pub fn fresh(&self) -> Self {
        Self::new(
            self.available.iter().copied().collect(),
            self.permitted.iter().copied().collect(),
            vec![],
        )
        .expect("empty active set")
    }
    pub fn catalog_with_authority(&self, unrestricted: bool) -> serde_json::Value {
        if !unrestricted {
            return self.catalog();
        }
        serde_json::json!(
            CapabilityId::ALL
                .into_iter()
                .map(|id| serde_json::json!({
                    "id": id.as_str(), "description": id.description(),
                    "available": self.available(id), "permitted": self.available(id),
                    "exposed": self.available(id),
                }))
                .collect::<Vec<_>>()
        )
    }

    pub fn catalog(&self) -> serde_json::Value {
        serde_json::json!(Self::catalog_entries(self))
    }
    fn catalog_entries(&self) -> Vec<serde_json::Value> {
        CapabilityId::ALL.into_iter().map(|id| serde_json::json!({
            "id": id.as_str(), "description": id.description(),
            "available": self.available.contains(&id), "permitted": self.permitted.contains(&id),
            "exposed": self.exposed(id),
        })).collect()
    }
    pub async fn enable(&self, id: CapabilityId) -> Result<bool, String> {
        tracing::info!(
            capability = id.as_str(),
            event = "capability_enable_requested"
        );
        if !self.available.contains(&id) {
            return Err(format!("capability unavailable: {}", id.as_str()));
        }
        if !self.permitted.contains(&id) {
            return Err(format!("capability not permitted: {}", id.as_str()));
        }
        let _guard = self.enable_gate.lock().await;
        if self.exposed(id) {
            return Ok(false);
        }
        let binding = self.binding.read().expect("capability lock").clone();
        if let Some(binding) = binding {
            binding
                .store
                .enable_capability(&binding.token, &binding.goal, id.as_str())
                .await
                .map_err(|error| format!("capability persistence failed: {error}"))?;
        }
        self.active.write().expect("capability lock").insert(id);
        tracing::info!(capability = id.as_str(), event = "capability_enabled");
        Ok(true)
    }
    pub async fn bind_goal(
        &self,
        store: Arc<dyn GoalStore>,
        token: OwnershipToken,
        goal: GoalId,
    ) -> Result<(), String> {
        let _guard = self.enable_gate.lock().await;
        let restored = store
            .active_capabilities(&goal)
            .await
            .map_err(|e| e.to_string())?;
        let active: BTreeSet<_> = restored
            .iter()
            .map(|id| CapabilityId::parse(id))
            .collect::<Result<_, _>>()?;
        if !active.is_subset(&self.available) || !active.is_subset(&self.permitted) {
            return Err("restored capability unavailable or not permitted".into());
        }
        *self.active.write().expect("capability lock") = active;
        *self.binding.write().expect("capability lock") = Some(GoalBinding { store, token, goal });
        tracing::info!(event = "tool_surface_changed", reason = "resume_restored", active_capabilities = ?self.active());
        Ok(())
    }
    pub fn reset(&self) {
        self.active.write().expect("capability lock").clear();
        *self.binding.write().expect("capability lock") = None;
    }
}

pub fn control_definition() -> ToolDefinition {
    ToolDefinition {
        name: "capability".into(),
        description: "Discover optional capabilities (list/status); enable a needed capability for subsequent requests.".into(),
        input_schema: serde_json::json!({"type":"object","properties":{
            "action":{"type":"string","enum":["list","status","enable"]},
            "id":{"type":"string"}},"required":["action"],"additionalProperties":false}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn enabled_pack_exposes_only_its_tools_and_is_idempotent() {
        let state = CapabilityDisclosure::new(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL.to_vec(),
            vec![],
        )
        .unwrap();
        assert!(!state.permits_tool("browser_tab"));
        assert!(state.permits_tool("run_command"));
        assert!(state.enable(CapabilityId::Browser).await.unwrap());
        assert!(state.permits_tool("browser_tab"));
        assert!(!state.permits_tool("spawn_agent"));
        assert!(!state.enable(CapabilityId::Browser).await.unwrap());
        assert!(state.fresh().active().is_empty());
    }
    #[tokio::test]
    async fn full_capability_authority_bypasses_permission_but_not_availability() {
        let state = CapabilityDisclosure::new(vec![CapabilityId::Memory], vec![], vec![]).unwrap();
        assert!(
            state
                .enable_with_authority(CapabilityId::Memory, false)
                .await
                .is_err()
        );
        assert!(
            !state
                .enable_with_authority(CapabilityId::Memory, true)
                .await
                .unwrap()
        );
        assert!(
            state
                .enable_with_authority(CapabilityId::Browser, true)
                .await
                .unwrap_err()
                .contains("unavailable")
        );
        assert!(
            state.active().is_empty(),
            "Full permission bypass must not persist a grant"
        );
    }

    #[tokio::test]
    async fn availability_and_permission_both_gate_enable() {
        let state = CapabilityDisclosure::new(vec![CapabilityId::Memory], vec![], vec![]).unwrap();
        assert!(
            state
                .enable(CapabilityId::Browser)
                .await
                .unwrap_err()
                .contains("unavailable")
        );
        assert!(
            state
                .enable(CapabilityId::Memory)
                .await
                .unwrap_err()
                .contains("not permitted")
        );
        assert!(state.active().is_empty());
    }
    #[test]
    fn delegation_does_not_expose_authoring_and_catalog_is_small() {
        let state = CapabilityDisclosure::new(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL.to_vec(),
            vec![CapabilityId::MultiAgent],
        )
        .unwrap();
        assert!(state.permits_tool("spawn_agent"));
        assert!(!state.permits_tool("save_agent"));
        assert!(
            leveler_model::estimate_text(&state.catalog().to_string())
                + leveler_model::estimate_tool_definitions(&[control_definition()])
                < 1000
        );
    }
    #[tokio::test]
    async fn child_loads_share_goal_truth_but_keep_role_capability_bounds() {
        let parent = CapabilityDisclosure::new(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL.to_vec(),
            vec![CapabilityId::Browser],
        )
        .unwrap();
        let mut memory = control_definition();
        memory.name = "memory".into();
        let child = parent.for_child(&[memory]);
        assert!(!child.exposed(CapabilityId::Browser));
        assert!(child.enable(CapabilityId::Authoring).await.is_err());
        assert!(child.enable(CapabilityId::Memory).await.unwrap());
        assert!(parent.exposed(CapabilityId::Memory));
        assert!(!child.permits_tool("browser_tab"));
    }
}
