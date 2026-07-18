//! Read-only planning: understand → localize → plan (spec §22-27).
//!
//! Execution moved to `leveler-engine`'s plan strategy — every node runs as a
//! fully-persisted engine turn there. What remains here is the planning
//! pipeline behind `leveler plan` (no edits, no commands).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_context::ContextPackage;
use leveler_model::{ModelRef, ModelRuntime};
use leveler_tools::{ToolContext, ToolRegistry};

use crate::error::OrchestratorError;
use crate::graph::TaskGraph;
use crate::planner::Planner;
use crate::requirement::Requirement;

/// Events emitted as the planning pipeline advances.
#[derive(Debug, Clone)]
#[allow(clippy::enum_variant_names)]
pub enum OrchestratorEvent {
    RequirementReady(Box<Requirement>),
    /// The context package (candidate files, token estimate) is ready.
    ContextReady {
        candidate_files: Vec<String>,
        estimated_tokens: u32,
    },
    PlanReady(Box<TaskGraph>),
}

/// Drives the read-only planning pipeline.
pub struct Orchestrator {
    runtime: Arc<dyn ModelRuntime>,
    registry: Arc<ToolRegistry>,
    tool_context: ToolContext,
    model: ModelRef,
}

impl Orchestrator {
    pub fn new(
        runtime: Arc<dyn ModelRuntime>,
        registry: Arc<ToolRegistry>,
        tool_context: ToolContext,
        model: ModelRef,
    ) -> Self {
        Self {
            runtime,
            registry,
            tool_context,
            model,
        }
    }

    /// The observer-free planning brain, shared with the engine (plan B5).
    fn planner(&self) -> Planner {
        Planner {
            runtime: self.runtime.clone(),
            registry: self.registry.clone(),
            tool_context: self.tool_context.clone(),
            model: self.model.clone(),
        }
    }

    /// Understand → Localize → Plan. No edits are made.
    pub async fn plan_only(
        &self,
        goal: &str,
        observer: &mut dyn FnMut(OrchestratorEvent),
        cancellation: &CancellationToken,
    ) -> Result<(Requirement, TaskGraph), OrchestratorError> {
        let requirement = self.understand(goal, observer, cancellation).await?;
        let context = self.localize(&requirement.goal, observer);
        let graph = self
            .plan(&requirement, &context, observer, cancellation)
            .await?;
        Ok((requirement, graph))
    }

    async fn understand(
        &self,
        goal: &str,
        observer: &mut dyn FnMut(OrchestratorEvent),
        cancellation: &CancellationToken,
    ) -> Result<Requirement, OrchestratorError> {
        let requirement = self.planner().understand(goal, cancellation).await?;
        observer(OrchestratorEvent::RequirementReady(Box::new(
            requirement.clone(),
        )));
        Ok(requirement)
    }

    fn localize(&self, goal: &str, observer: &mut dyn FnMut(OrchestratorEvent)) -> ContextPackage {
        let context = self.planner().localize(goal);
        observer(OrchestratorEvent::ContextReady {
            candidate_files: context.candidate_files.clone(),
            estimated_tokens: context.estimated_tokens,
        });
        context
    }

    async fn plan(
        &self,
        requirement: &Requirement,
        context: &ContextPackage,
        observer: &mut dyn FnMut(OrchestratorEvent),
        cancellation: &CancellationToken,
    ) -> Result<TaskGraph, OrchestratorError> {
        let graph = self
            .planner()
            .plan(requirement, context, cancellation)
            .await?;
        observer(OrchestratorEvent::PlanReady(Box::new(graph.clone())));
        Ok(graph)
    }
}
