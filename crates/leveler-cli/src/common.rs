//! Shared CLI helpers: model resolution, mode mapping, approver construction,
//! and the Ctrl+C interrupt handler.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use leveler_app::Application;
use leveler_execution::{Approver, AutoApprove, PermissionProfile};
use leveler_model::ModelRef;

use crate::approver;
use crate::cli::RunMode;
use crate::output::Line;

/// Resolve the model reference: CLI flag, else `.leveler/config.yaml` default,
/// else the single configured model.
pub(crate) fn resolve_model(app: &Application, model: Option<String>) -> anyhow::Result<ModelRef> {
    if let Some(m) = model {
        return parse_model_ref(&m);
    }
    if let Some(m) = app
        .project_config()
        .model
        .as_deref()
        .and_then(ModelRef::parse)
    {
        return Ok(m);
    }
    // Global config default (~/.leveler/config.toml).
    if let Some(m) = app
        .config
        .default_model
        .as_deref()
        .and_then(ModelRef::parse)
    {
        return Ok(m);
    }
    let mut refs = app.model_refs();
    refs.sort_by_key(|r| r.to_string());
    refs.into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no models configured; add one under configs/models/"))
}

pub(crate) fn map_mode(mode: RunMode) -> PermissionProfile {
    match mode {
        RunMode::RequestApproval => PermissionProfile::RequestApproval,
        RunMode::Assisted => PermissionProfile::Assisted,
        RunMode::FullAccess => PermissionProfile::FullAccess,
    }
}

pub(crate) fn build_approver(auto_approve: bool) -> Arc<dyn Approver> {
    if auto_approve {
        Arc::new(AutoApprove)
    } else {
        Arc::new(approver::CliApprover)
    }
}

/// Install a Ctrl+C handler that cancels the run gracefully (once).
pub(crate) fn spawn_interrupt_handler(token: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!(
                "\n{}",
                Line::warn("Interrupt received — cancelling current step…")
            );
            token.cancel();
        }
    });
}

pub(crate) fn parse_model_ref(model: &str) -> anyhow::Result<ModelRef> {
    ModelRef::parse(model).ok_or_else(|| {
        anyhow::anyhow!("invalid model reference `{model}` (expected `provider/model`)")
    })
}
