//! Model-requested disclosure. Availability and permission are host facts;
//! activated packs are session exposure and never inferred from task text.
//!
//! `capability(enable)` promises the tools on the following request, so the
//! activation must outlive the disclosure of one turn: every turn builds a
//! fresh disclosure from the host facts of that turn, and reads its activated
//! packs from the session's shared cell. A goal contributes its durable
//! activations to that cell; it never removes one, because exposure is not
//! authority and losing it silently is what made the model contradict itself.
use std::collections::{BTreeMap, BTreeSet};
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
    ComputerUse,
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
        Self::ComputerUse,
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
            Self::ComputerUse => "computer_use",
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
            Self::ComputerUse => "Observe/control the desktop",
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
        "computer_observe" | "computer_act" => Some(CapabilityId::ComputerUse),
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

/// The activated packs of one SESSION, shared by every turn's disclosure.
///
/// A turn's engine is built and dropped per turn, and it must not own the
/// answer to "which packs did the model enable": the runtime told the model
/// those tools are available on the following request, and the following
/// request can belong to a later turn. This cell is that one owner.
///
/// Two facts live here, because they have two authorities:
///
/// - `session`: activations no goal record owns — a Chat turn's enable, or a
///   Full-authority enable that deliberately writes no grant. They last for the
///   session.
/// - `goal`: what the bound goal contributes — its durable rows plus
///   activations made while it was bound. Binding another goal replaces this
///   set, so a new goal still starts with a small surface.
///
/// Availability and permission are not here: every turn recomputes them from
/// its own host facts, so a narrowed profile still gates what this cell holds.
#[derive(Clone, Default)]
pub struct SharedCapabilityActivation {
    session: Arc<RwLock<BTreeSet<CapabilityId>>>,
    goal: Arc<RwLock<BTreeSet<CapabilityId>>>,
}

impl SharedCapabilityActivation {
    pub fn new(active: impl IntoIterator<Item = CapabilityId>) -> Self {
        Self {
            session: Arc::new(RwLock::new(active.into_iter().collect())),
            goal: Arc::new(RwLock::new(BTreeSet::new())),
        }
    }

    fn contains(&self, id: CapabilityId) -> bool {
        self.session.read().expect("capability lock").contains(&id)
            || self.goal.read().expect("capability lock").contains(&id)
    }

    /// The union, read-only: asking which packs are active must not move one
    /// fact into the other's set.
    fn ids(&self) -> Vec<CapabilityId> {
        let session: BTreeSet<_> = self.session.read().expect("capability lock").clone();
        let goal: BTreeSet<_> = self.goal.read().expect("capability lock").clone();
        session.union(&goal).copied().collect()
    }

    fn insert_session(&self, id: CapabilityId) -> bool {
        self.session.write().expect("capability lock").insert(id)
    }

    fn insert_goal(&self, id: CapabilityId) -> bool {
        self.goal.write().expect("capability lock").insert(id)
    }

    /// The bound goal's activation is exactly its record plus what this turn
    /// activated on it. A different goal replaces it; the session's own
    /// activations are untouched.
    fn replace_goal(&self, next: BTreeSet<CapabilityId>) {
        *self.goal.write().expect("capability lock") = next;
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
    /// Packs that became available DURING this turn — an external catalog whose
    /// server started while the turn was running. Availability is a host fact
    /// that can change mid-turn; the turn's surface must be able to say so
    /// without pretending the pack was offered from the beginning.
    late_available: RwLock<BTreeSet<CapabilityId>>,
    permitted: BTreeSet<CapabilityId>,
    /// Why a pack is not available on THIS turn, in the host's words. A pack
    /// whose tools could not be reached (an MCP server that is not up yet) looks
    /// identical to one that was never configured unless the difference is
    /// stated; the model must be able to tell "not offered" from "not ready".
    unavailable: RwLock<BTreeMap<CapabilityId, String>>,
    /// Capabilities that are part of the product's first-round surface. They
    /// are exposed without an enable round trip and are never persisted as a
    /// grant, so a goal restore cannot take them away.
    default_active: BTreeSet<CapabilityId>,
    active: SharedCapabilityActivation,
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
            late_available: RwLock::new(BTreeSet::new()),
            permitted,
            unavailable: RwLock::new(BTreeMap::new()),
            default_active: BTreeSet::new(),
            active: SharedCapabilityActivation::new(active),
            binding: RwLock::new(None),
            enable_gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Build the disclosure of one turn over a session's shared activation.
    ///
    /// Unlike [`Self::new`], an activation the CURRENT turn's host facts do not
    /// support is not an error: the mode, the permission profile or a project
    /// switch may have narrowed since the enable. The cell keeps it, and
    /// [`Self::exposed`] reports the truth for this request.
    pub fn for_session(
        available: Vec<CapabilityId>,
        permitted: Vec<CapabilityId>,
        activation: SharedCapabilityActivation,
    ) -> Self {
        Self {
            available: available.into_iter().collect(),
            late_available: RwLock::new(BTreeSet::new()),
            permitted: permitted.into_iter().collect(),
            unavailable: RwLock::new(BTreeMap::new()),
            default_active: BTreeSet::new(),
            active: activation,
            binding: RwLock::new(None),
            enable_gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Record, for this turn only, why an otherwise-offered pack has nothing to
    /// offer. It never changes availability or permission.
    pub fn with_unavailable(self, reasons: BTreeMap<CapabilityId, String>) -> Self {
        *self.unavailable.write().expect("capability lock") = reasons;
        self
    }

    /// Mark capabilities as part of the product's default surface.
    ///
    /// They must still be mechanically available and permitted; a default is
    /// exposure, not authority, so it can never widen what the host supports.
    pub fn with_default_active(mut self, defaults: Vec<CapabilityId>) -> Result<Self, String> {
        for id in defaults {
            if !self.available(id) || !self.permitted.contains(&id) {
                return Err(format!(
                    "default capability unavailable or not permitted: {}",
                    id.as_str()
                ));
            }
            self.default_active.insert(id);
        }
        Ok(self)
    }

    pub fn available(&self, id: CapabilityId) -> bool {
        self.available.contains(&id)
            || self
                .late_available
                .read()
                .expect("capability lock")
                .contains(&id)
    }

    /// Whether this host permits the pack at all.
    ///
    /// Distinct from [`Self::available`] (the tools exist) and from
    /// [`Self::exposed`] (they are on this request's surface): only the
    /// conjunction of the three is a pack the model could obtain right now.
    pub fn is_permitted(&self, id: CapabilityId) -> bool {
        self.permitted.contains(&id)
    }

    /// A pack whose tools arrived during this turn. It is offered from now on;
    /// the operator (permission) fact does not change.
    pub fn note_available(&self, id: CapabilityId) {
        if self.available.contains(&id) {
            return;
        }
        self.unavailable
            .write()
            .expect("capability lock")
            .remove(&id);
        if self
            .late_available
            .write()
            .expect("capability lock")
            .insert(id)
        {
            tracing::info!(
                capability = id.as_str(),
                event = "capability_became_available"
            );
        }
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
            if !self.permitted.contains(&id) {
                return Err(format!("capability not permitted: {}", id.as_str()));
            }
            // Full removes the permission check, not disclosure. Activate the
            // pack for this session so its tools reach the following requests,
            // but never write a grant: exposure is not authority, and a resumed
            // Full session asks again through the same catalog. No goal record
            // owns this fact, so it joins the session's activations.
            return Ok(self.active.insert_session(id));
        }
        self.enable(id).await
    }

    pub fn active(&self) -> Vec<CapabilityId> {
        self.active
            .ids()
            .into_iter()
            .filter(|id| self.available(*id) && self.permitted.contains(id))
            .collect()
    }
    pub fn exposed(&self, id: CapabilityId) -> bool {
        self.available(id)
            && self.permitted.contains(&id)
            && (self.default_active.contains(&id) || self.active.contains(id))
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
            late_available: RwLock::new(BTreeSet::new()),
            permitted: self.permitted.intersection(&offered).copied().collect(),
            unavailable: RwLock::new(self.unavailable.read().expect("capability lock").clone()),
            default_active: self
                .default_active
                .intersection(&offered)
                .copied()
                .collect(),
            active: self.active.clone(),
            binding: RwLock::new(self.binding.read().expect("capability lock").clone()),
            enable_gate: self.enable_gate.clone(),
        }
    }
    pub fn fresh(&self) -> Self {
        Self::new(
            self.available
                .iter()
                .copied()
                .chain(
                    self.late_available
                        .read()
                        .expect("capability lock")
                        .iter()
                        .copied(),
                )
                .collect(),
            self.permitted.iter().copied().collect(),
            vec![],
        )
        .expect("empty active set")
        .with_default_active(self.default_active.iter().copied().collect())
        .expect("defaults were valid on the parent")
    }
    /// The catalog the model reads. `exposed` is the truth for this request;
    /// `permitted` is the authority fact. FullAccess makes every available
    /// capability permitted, but an unenabled pack is still not exposed.
    pub fn catalog_with_authority(&self, _unrestricted: bool) -> serde_json::Value {
        self.catalog()
    }

    pub fn catalog(&self) -> serde_json::Value {
        serde_json::json!(Self::catalog_entries(self))
    }
    fn catalog_entries(&self) -> Vec<serde_json::Value> {
        CapabilityId::ALL
            .into_iter()
            .map(|id| {
                let mut entry = serde_json::json!({
                    "id": id.as_str(), "description": id.description(),
                    "available": self.available(id),
                    "permitted": self.permitted.contains(&id),
                    "exposed": self.exposed(id),
                });
                let unavailable = self.unavailable.read().expect("capability lock");
                if let Some(reason) = unavailable.get(&id) {
                    entry["detail"] = serde_json::Value::String(reason.clone());
                }
                entry
            })
            .collect()
    }
    pub async fn enable(&self, id: CapabilityId) -> Result<bool, String> {
        tracing::info!(
            capability = id.as_str(),
            event = "capability_enable_requested"
        );
        if !self.available(id) {
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
        let Some(binding) = binding else {
            // No goal owns this activation: it is the session's.
            self.active.insert_session(id);
            tracing::info!(capability = id.as_str(), event = "capability_enabled");
            return Ok(true);
        };
        binding
            .store
            .enable_capability(&binding.token, &binding.goal, id.as_str())
            .await
            .map_err(|error| format!("capability persistence failed: {error}"))?;
        self.active.insert_goal(id);
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
            .filter(|id| id.as_str() != "memory")
            .map(|id| CapabilityId::parse(id))
            .collect::<Result<_, _>>()?;
        let available: BTreeSet<_> = self
            .available
            .iter()
            .copied()
            .chain(
                self.late_available
                    .read()
                    .expect("capability lock")
                    .iter()
                    .copied(),
            )
            .collect();
        if !active.is_subset(&available) || !active.is_subset(&self.permitted) {
            return Err("restored capability unavailable or not permitted".into());
        }
        // The bound goal's durable activations replace the previous goal's, so
        // a new goal never inherits an older one's pack. This session's own
        // activations are untouched.
        self.active.replace_goal(active);
        *self.binding.write().expect("capability lock") = Some(GoalBinding { store, token, goal });
        tracing::info!(event = "tool_surface_changed", reason = "resume_restored", active_capabilities = ?self.active());
        Ok(())
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
    async fn legacy_goal_activation_skips_only_the_retired_pack() {
        use leveler_core::{BootId, OwnerEpoch, RuntimeId, TaskId};
        use leveler_storage::{
            MemoryGoalStore, MemoryOwnershipState, MemoryOwnershipStore, OwnershipStore,
        };
        let ownership = Arc::new(MemoryOwnershipState::new());
        let task = TaskId::new("legacy-capability-goal");
        ownership.register_task(&task);
        let token = MemoryOwnershipStore::new(ownership.clone())
            .acquire(
                &task,
                &RuntimeId::new("runtime"),
                &BootId::new("boot"),
                OwnerEpoch::UNOWNED,
            )
            .await
            .unwrap();
        let store = Arc::new(MemoryGoalStore::new().with_ownership(ownership));
        let goal = store
            .open(&token, "resume work", leveler_core::now())
            .await
            .unwrap();
        store
            .enable_capability(&token, &goal, "memory")
            .await
            .unwrap();
        store
            .enable_capability(&token, &goal, "browser")
            .await
            .unwrap();
        let disclosure = CapabilityDisclosure::new(
            vec![CapabilityId::Browser],
            vec![CapabilityId::Browser],
            vec![],
        )
        .unwrap();
        disclosure
            .bind_goal(store.clone(), token.clone(), goal.clone())
            .await
            .unwrap();
        assert_eq!(disclosure.active(), vec![CapabilityId::Browser]);
        store
            .enable_capability(&token, &goal, "unknown_future_pack")
            .await
            .unwrap();
        assert!(
            disclosure
                .bind_goal(store, token, goal)
                .await
                .unwrap_err()
                .contains("unknown_future_pack")
        );
    }

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
    async fn full_capability_authority_activates_exposure_without_persisting_a_grant() {
        // Production resolves `permitted` from `available` in FullAccess; Web
        // stays available-but-unpermitted here to prove disclosure still obeys
        // the host's authority fact.
        let state = CapabilityDisclosure::new(
            vec![CapabilityId::Skills, CapabilityId::Web],
            vec![CapabilityId::Skills],
            vec![],
        )
        .unwrap();
        assert!(
            state
                .enable_with_authority(CapabilityId::Web, false)
                .await
                .unwrap_err()
                .contains("not permitted")
        );
        assert!(
            state
                .enable_with_authority(CapabilityId::Skills, true)
                .await
                .unwrap(),
            "Full activates an available capability without approval"
        );
        assert!(state.exposed(CapabilityId::Skills));
        assert!(
            !state
                .enable_with_authority(CapabilityId::Skills, true)
                .await
                .unwrap(),
            "a second enable is idempotent"
        );
        assert!(
            state
                .enable_with_authority(CapabilityId::Browser, true)
                .await
                .unwrap_err()
                .contains("unavailable")
        );
        assert!(
            state.fresh().active().is_empty(),
            "the activation is session exposure, not a persisted grant"
        );
    }

    #[tokio::test]
    async fn a_default_capability_needs_no_enable_round_trip_and_is_not_a_grant() {
        let session = SharedCapabilityActivation::default();
        let default_turn = |activation: SharedCapabilityActivation| {
            CapabilityDisclosure::for_session(
                vec![CapabilityId::Skills],
                vec![CapabilityId::Skills],
                activation,
            )
            .with_default_active(vec![CapabilityId::Skills])
            .unwrap()
        };
        let state = default_turn(session.clone());
        assert!(
            state.exposed(CapabilityId::Skills),
            "the product's default surface is visible on the first request"
        );
        assert!(state.permits_tool("load_skill"));
        assert!(
            !state.enable(CapabilityId::Skills).await.unwrap(),
            "a default is already exposed; enabling it is a no-op, not a new grant"
        );
        // The next turn builds its own disclosure over the same session cell.
        let next = default_turn(session);
        assert!(
            next.exposed(CapabilityId::Skills),
            "the default surface is not something a turn boundary can take away"
        );
        assert!(
            next.active().is_empty(),
            "a default must never be reported as a persisted grant"
        );
        assert!(state.fresh().exposed(CapabilityId::Skills));
    }

    #[tokio::test]
    async fn a_default_capability_cannot_be_permitted_or_available_by_itself() {
        let unavailable = CapabilityDisclosure::new(vec![], vec![], vec![]).unwrap();
        assert!(
            unavailable
                .with_default_active(vec![CapabilityId::Skills])
                .is_err()
        );
        let unpermitted =
            CapabilityDisclosure::new(vec![CapabilityId::Skills], vec![], vec![]).unwrap();
        assert!(
            unpermitted
                .with_default_active(vec![CapabilityId::Skills])
                .is_err()
        );
    }

    #[tokio::test]
    async fn availability_and_permission_both_gate_enable() {
        let state = CapabilityDisclosure::new(vec![CapabilityId::Skills], vec![], vec![]).unwrap();
        assert!(
            state
                .enable(CapabilityId::Browser)
                .await
                .unwrap_err()
                .contains("unavailable")
        );
        assert!(
            state
                .enable(CapabilityId::Skills)
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
        let mut skill = control_definition();
        skill.name = "load_skill".into();
        let child = parent.for_child(&[skill]);
        assert!(!child.exposed(CapabilityId::Browser));
        assert!(child.enable(CapabilityId::Authoring).await.is_err());
        assert!(child.enable(CapabilityId::Skills).await.unwrap());
        assert!(parent.exposed(CapabilityId::Skills));
        assert!(!child.permits_tool("browser_tab"));
    }

    /// A turn's engine is thrown away; the session's activation is not. This
    /// is the surface a real TUI run saw vanish one turn after the enable.
    #[tokio::test]
    async fn a_session_activation_outlives_the_disclosure_of_one_turn() {
        let session = SharedCapabilityActivation::default();
        let turn_one = CapabilityDisclosure::for_session(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL.to_vec(),
            session.clone(),
        );
        assert!(!turn_one.permits_tool("spawn_agent"));
        assert!(turn_one.enable(CapabilityId::MultiAgent).await.unwrap());

        let turn_two = CapabilityDisclosure::for_session(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL.to_vec(),
            session.clone(),
        );
        assert!(
            turn_two.permits_tool("spawn_agent"),
            "the next turn's request must carry the pack the previous turn enabled"
        );
        let catalog = turn_two.catalog();
        let entry = catalog
            .as_array()
            .expect("the catalog is a list")
            .iter()
            .find(|entry| entry["id"] == "multi_agent")
            .expect("multi_agent is in the catalog");
        assert_eq!(entry["exposed"].as_bool(), Some(true));

        // Narrowed host facts gate it for this turn without destroying the
        // activation: authority is recomputed, exposure is remembered.
        let narrowed = CapabilityDisclosure::for_session(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL
                .into_iter()
                .filter(|id| *id != CapabilityId::MultiAgent)
                .collect(),
            session.clone(),
        );
        assert!(!narrowed.permits_tool("spawn_agent"));
        assert!(
            narrowed
                .enable(CapabilityId::MultiAgent)
                .await
                .unwrap_err()
                .contains("not permitted")
        );

        let turn_three = CapabilityDisclosure::for_session(
            CapabilityId::ALL.to_vec(),
            CapabilityId::ALL.to_vec(),
            session,
        );
        assert!(
            turn_three.permits_tool("spawn_agent"),
            "a gate that lifted must not have silently erased the activation"
        );
    }

    /// Goal activation stays scoped to its goal: a new goal starts small and
    /// does not inherit the previous goal's packs, while the session's own
    /// activations are not the goal's to take away.
    #[test]
    fn a_new_goal_replaces_the_goals_packs_and_keeps_the_sessions_own() {
        let session = SharedCapabilityActivation::default();
        session.replace_goal([CapabilityId::Skills].into_iter().collect());
        assert!(session.contains(CapabilityId::Skills));

        session.insert_session(CapabilityId::MultiAgent);
        session.replace_goal(BTreeSet::new());
        assert!(
            !session.contains(CapabilityId::Skills),
            "a new goal must not inherit the previous goal's activation"
        );
        assert!(
            session.contains(CapabilityId::MultiAgent),
            "the goal rebind is not the session's to revoke"
        );
        assert_eq!(session.ids(), vec![CapabilityId::MultiAgent]);
    }
}
