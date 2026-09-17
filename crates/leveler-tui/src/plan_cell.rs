use leveler_client_protocol::PlanStepStatus;

/// The glyph for a plan step (never color-only, spec §31.1). Shared with the
/// workbench plan dock so one state never reads as two different things.
pub(crate) fn plan_glyph(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Pending => "○",
        PlanStepStatus::Running => "●",
        PlanStepStatus::Done => "✓",
        PlanStepStatus::Failed => "✗",
        PlanStepStatus::Skipped => "–",
    }
}
