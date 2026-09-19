//! Status and bottom-bar rendering with sparse chrome.
//!
//! Layout contract (matches the product screenshot target):
//!
//! - **Header** (1 line): branch · repo path (muted; identity only)
//! - **Notice** (1 line): the transient Notice Surface for user-action feedback
//!   (only when there is one; see `workbench::render_notice`)
//! - **Status** (1 line): live activity only (empty when idle)
//! - **Input border**: `{model} [(effort)] · work-mode · permission · session`
//! - **Footer** (1 line): runtime context + local wall clock — `Context 8k/1M · 22:22`
//!
//! Vertical breathing (workbench): blank above the input when status/queue/plan
//! chrome is visible; blank between input and the Context footer always.
//!
//! Shortcuts are not sticky chrome — discover via `/help` or `Ctrl+?`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::{FinalizationStage, RuntimeStatus};

use crate::state::AppState;
use crate::transcript::TranscriptItem;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) fn fmt_elapsed(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Local wall clock as `HH:MM`: 24-hour, no seconds, no date, no label.
///
/// The timezone is already baked into `time` by the caller; this only fixes
/// the presentation format so it can be tested without touching the clock.
pub(crate) fn fmt_clock(time: chrono::NaiveTime) -> String {
    time.format("%H:%M").to_string()
}

pub(crate) fn fmt_tokens(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn estimate_tokens(text: &str) -> u32 {
    let (mut cjk, mut other) = (0u32, 0u32);
    for ch in text.chars() {
        if ch as u32 >= 0x2E80 {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    (cjk as f32 / 1.6 + other as f32 / 4.0).ceil() as u32
}

/// Live output of the round currently streaming — visible answer text AND the
/// hidden reasoning.
///
/// Reasoning has to count. A measured round on a thinking model ran 95s and
/// produced 6738 output tokens of which only 45 characters were answer text:
/// ignoring reasoning here is what made a hard-working turn look frozen.
fn streaming_output_estimate(state: &AppState) -> Option<u32> {
    let visible = state
        .transcript
        .items()
        .iter()
        .rev()
        .find_map(|it| match it {
            TranscriptItem::Assistant(b) if !b.done => Some(b.text.as_str()),
            _ => None,
        })
        .map(estimate_tokens)
        .unwrap_or(0);
    let thinking = if state.live_reasoning.is_empty() {
        0
    } else {
        estimate_tokens(&state.live_reasoning)
    };
    let total = visible.saturating_add(thinking);
    (total > 0).then_some(total)
}

/// Permission mode label and color role (localized chrome, e.g. status screens).
/// Stable English permission chip for the Input border (`ask` / `auto` / `full`).
///
/// These are the user-facing names of [`PermissionProfile`], not inferred
/// aliases. There is no standing `deny` profile.
pub(crate) fn permission_chip_label(state: &AppState) -> &'static str {
    match state.mode_label.as_str() {
        "RequestApproval" => "ask",
        "FullAccess" => "full",
        _ => "auto",
    }
}

/// Strip `provider/` when the model name is unambiguous among `available`.
pub(crate) fn friendly_model_label(
    raw: &str,
    available: &[leveler_client_protocol::ModelRef],
) -> String {
    let Some(parsed) = leveler_client_protocol::ModelRef::parse(raw) else {
        return raw.to_string();
    };
    let collisions = available.iter().filter(|m| m.model == parsed.model).count();
    if collisions > 1 {
        raw.to_string()
    } else {
        parsed.model
    }
}

/// Input-border runtime summary: `{model} [(effort)] · work · perm · session`.
///
/// Every field is a value already on [`AppState`]. Missing `reasoning_effort`
/// omits the parentheses. When `max_width` is tight, drop from the right:
/// session, then permission, then work_mode, then effort, then truncate model.
pub(crate) fn runtime_status_chip(state: &AppState, max_width: usize) -> String {
    let model = friendly_model_label(&state.model_label, &state.available_models);
    let effort = state
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let perm = permission_chip_label(state);
    let untrusted = (!state.untrusted_config.is_empty()).then_some(state.t().untrusted_config_chip);

    let mut extras: Vec<String> = Vec::new();
    extras.push(state.work_profile.clone());
    extras.push(perm.to_string());
    extras.push(state.collaboration.clone());
    if let Some(u) = untrusted {
        extras.push(u.to_string());
    }

    let join = |head: &str, extra: &[String]| -> String {
        let mut parts = Vec::with_capacity(1 + extra.len());
        if !head.is_empty() {
            parts.push(head.to_string());
        }
        parts.extend(extra.iter().cloned());
        parts.join(" · ")
    };

    let head = match effort {
        Some(e) => format!("{model} ({e})"),
        None => model.clone(),
    };
    let mut shown = extras;
    loop {
        let chip = join(&head, &shown);
        if UnicodeWidthStr::width(chip.as_str()) <= max_width || shown.is_empty() {
            if UnicodeWidthStr::width(chip.as_str()) <= max_width {
                return chip;
            }
            break;
        }
        shown.pop();
    }

    // Drop effort next, then width-safe truncate the model name.
    if effort.is_some() {
        let chip = join(&model, &[]);
        if UnicodeWidthStr::width(chip.as_str()) <= max_width {
            return chip;
        }
    }
    crate::render::truncate_display(&model, max_width.max(1))
}

/// Compact magnitude for footer context: `41181 → 41k`, `1048576 → 1M`.
pub(crate) fn fmt_tokens_compact(n: u32) -> String {
    if n >= 1_000_000 {
        let whole = n / 1_000_000;
        let frac = (n % 1_000_000) / 100_000;
        if frac == 0 {
            format!("{whole}M")
        } else {
            format!("{whole}.{frac}M")
        }
    } else if n >= 10_000 {
        format!("{}k", n / 1_000)
    } else if n >= 1_000 {
        let whole = n / 1_000;
        let frac = (n % 1_000) / 100;
        if frac == 0 {
            format!("{whole}k")
        } else {
            format!("{whole}.{frac}k")
        }
    } else {
        n.to_string()
    }
}

/// Footer context line: `Context 8k/1M`. Hidden until real usage is known.
pub(crate) fn footer_ctx_chip(state: &AppState) -> Option<String> {
    let window = state.context_window();
    if window == 0 {
        return None;
    }
    let used = state.context_tokens.max(state.token_input);
    // Fresh session with zero usage: hide — don't show a fake 0/window gauge.
    if used == 0 {
        return None;
    }
    Some(
        state
            .t()
            .footer_context
            .replacen("{}", &fmt_tokens_compact(used), 1)
            .replacen("{}", &fmt_tokens_compact(window), 1),
    )
}

/// Prefix-cache hit rate when the provider reported cached tokens.
///
/// `None` when there is nothing to show (no input or no cache hits).
pub(crate) fn footer_cache_chip(state: &AppState) -> Option<String> {
    let input = state.token_input;
    let cached = state.token_cached;
    if input == 0 || cached == 0 {
        return None;
    }
    let pct = (cached as u64 * 100 / input as u64).min(100);
    Some(state.t().footer_cache.replace("{}", &pct.to_string()))
}

/// Footer usage line: `Context 21k/1M · cache 42%` — each chip optional.
///
/// Hidden until real usage is known. The wall clock is a separate cell placed
/// by the row's owner (`workbench::render_footer`), which owns the width budget
/// and the clock's priority; this only reports the usage chips.
pub(crate) fn footer_usage_line(state: &AppState) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(ctx) = footer_ctx_chip(state) {
        parts.push(ctx);
    }
    if let Some(cache) = footer_cache_chip(state) {
        parts.push(cache);
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn truncate_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(s) <= width {
        return s.to_string();
    }
    let mut acc = String::new();
    for ch in s.chars() {
        let next = format!("{acc}{ch}");
        if UnicodeWidthStr::width(next.as_str()) > width.saturating_sub(1) {
            acc.push('…');
            break;
        }
        acc.push(ch);
    }
    acc
}

fn fit_status(parts: &[String], width: usize) -> String {
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        let candidate = if i == 0 {
            p.clone()
        } else {
            format!("{out} · {p}")
        };
        if UnicodeWidthStr::width(candidate.as_str()) > width {
            break;
        }
        out = candidate;
    }
    truncate_to_width(&out, width)
}

fn turn_marker(
    text: String,
    color: ratatui::style::Color,
    width: usize,
    state: &AppState,
) -> Line<'static> {
    Line::from(Span::styled(
        truncate_to_width(&text, width),
        Style::default().fg(color).add_modifier(if state.is_busy() {
            Modifier::BOLD
        } else {
            Modifier::empty()
        }),
    ))
}

/// Coarse status-strip phase for honesty checks (tests + render).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusPhase {
    /// Nothing running; strip is empty or holds a stale activity label.
    Idle,
    /// Model / tools are working (may show spinner).
    Busy,
    /// Blocked on the human (approval, clarification, or similar overlay).
    AwaitingUser,
}

/// Which phase the workbench status strip should present.
///
/// Awaiting-user always wins over Busy so a spinner never implies the model is
/// still progressing while an approval/clarification overlay is open.
pub(crate) fn status_phase(state: &AppState) -> StatusPhase {
    if let Some(overlay) = &state.overlay {
        match overlay {
            crate::overlay::Overlay::Approval(_) | crate::overlay::Overlay::Clarification(_) => {
                return StatusPhase::AwaitingUser;
            }
            // Pickers are also user-blocked, but they are local chrome, not
            // runtime waits — still treat as awaiting-user for honesty.
            crate::overlay::Overlay::ModelPicker(_)
            | crate::overlay::Overlay::ModePicker(_)
            | crate::overlay::Overlay::ThemePicker(_)
            | crate::overlay::Overlay::WorkModePicker(_)
            | crate::overlay::Overlay::CollabPicker(_)
            | crate::overlay::Overlay::UnsupportedMedia(_)
            | crate::overlay::Overlay::CheckpointPicker(_) => {
                return StatusPhase::AwaitingUser;
            }
        }
    }
    match state.status {
        RuntimeStatus::Busy => StatusPhase::Busy,
        RuntimeStatus::Idle | RuntimeStatus::Error => StatusPhase::Idle,
    }
}

/// First status-strip row. Prefer [`status_lines`] when the wait disclosure
/// may occupy more than one row.
pub(crate) fn status_line_content(state: &AppState, width: usize) -> Line<'static> {
    status_lines(state, width)
        .into_iter()
        .next()
        .unwrap_or_else(|| Line::from(""))
}

/// Status strip: one headline, then a compact disclosure of the wait target.
pub(crate) fn status_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let theme = &state.theme;
    // Phase first: never paint a busy spinner while blocked on the user.
    match status_phase(state) {
        StatusPhase::AwaitingUser => {
            if let Some(overlay) = &state.overlay {
                if matches!(overlay, crate::overlay::Overlay::Clarification(_)) {
                    return vec![turn_marker(
                        state.t().waiting_reply.to_string(),
                        theme.accent.primary,
                        width,
                        state,
                    )];
                }
                // Interrupting overlays: static waiting copy, no spinner.
                // Pickers return None and fall through to the normal strip.
                if let Some(hint) = overlay.status_hint(state.t()) {
                    return vec![Line::from(Span::styled(
                        hint,
                        Style::default().fg(theme.status.warning),
                    ))];
                }
            }
        }
        StatusPhase::Busy | StatusPhase::Idle => {}
    }
    // Notices live on their own Notice Surface row above this strip (see
    // `workbench::render_notice`); never fold them into the status strip, so a
    // transient notice and persistent execution state stay separate rows.
    match state.status {
        RuntimeStatus::Busy => busy_status_lines(state, width),
        RuntimeStatus::Error | RuntimeStatus::Idle => {
            let mut lines = if let Some(label) = &state.activity {
                vec![Line::from(Span::styled(
                    format!("… {label}"),
                    Style::default().fg(theme.text.secondary),
                ))]
            } else {
                vec![Line::from("")]
            };
            // A background process outlives the turn that started it. Its
            // activity row — and a finished one kept for reopening — must stay
            // visible while Main is idle, or the only way back to its detail
            // would be a still-running turn.
            append_activity_rows(&mut lines, state, width, theme);
            lines
        }
    }
}

const WAIT_MARKER: &str = "◌";

/// `第 2/4 步进行中` for the live activity line, from the declared plan only.
///
/// The step shown is the one the plan declares in progress — never "the next
/// pending one", which would claim a step had started because the previous one
/// finished. No plan, or no step in progress, means no chip.
fn live_plan_step_chip(state: &AppState, t: &crate::i18n::UiText) -> Option<String> {
    let plan = state.plan.as_ref()?;
    let total = plan.steps.len();
    if total == 0 {
        return None;
    }
    let current = plan
        .steps
        .iter()
        .find(|s| s.status == leveler_client_protocol::PlanStepStatus::Running)?;
    Some(
        t.plan_running_chip
            .replace("{current}", &(current.index + 1).to_string())
            .replace("{total}", &total.to_string()),
    )
}

fn busy_status_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let theme = &state.theme;
    let t = state.t();
    let wait = crate::wait_status::project(state);
    if let Some(view) = wait.as_ref()
        && !matches!(
            view.kind,
            crate::wait_status::WaitKind::Model | crate::wait_status::WaitKind::Approval
        )
    {
        return blocked_wait_lines(state, view, width);
    }

    let frame = SPINNER[(state.tick as usize) % SPINNER.len()];
    let label = match state.finalization_stage {
        Some(FinalizationStage::SettlingDependencies) => t.finalizing_dependencies.to_string(),
        Some(FinalizationStage::Verification) => t.finalizing_verification.to_string(),
        Some(FinalizationStage::Evidence) => t.finalizing_evidence.to_string(),
        Some(FinalizationStage::Review) => t.finalizing_review.to_string(),
        Some(FinalizationStage::ResolvingOutcome) => t.finalizing_outcome.to_string(),
        Some(FinalizationStage::PublishingTerminal) => t.finalizing_terminal.to_string(),
        // Reasoning already arriving means the model is answering, not being
        // waited on. Same rule as a command heartbeat or a finalization stage:
        // never print a bare "等待模型" when a more specific stage is known.
        // Only that word is replaced — a running command still owns the line.
        None => {
            // A reconnect owns the line: the generic "waiting for model" would
            // hide the one fact the user needs during a network blip. The
            // countdown is the runtime's OWN announced delay, not a guess.
            if let Some(rc) = state.reconnecting {
                t.reconnecting
                    .replace("{attempt}", &rc.attempt.to_string())
                    .replace("{max}", &rc.max_attempts.to_string())
                    .replace("{secs}", &rc.remaining_secs().to_string())
            } else {
                let waiting_on_model = match wait.as_ref() {
                    Some(view) => view.kind == crate::wait_status::WaitKind::Model,
                    None => state.activity.is_none(),
                };
                // The transport accepted a fresh attempt after a retry. Brief
                // confirmation, then the normal streaming status returns. It
                // only stands in for the GENERIC model wait: a running command
                // or tool is the more specific fact and keeps the line.
                if waiting_on_model
                    && state
                        .reconnected_until
                        .is_some_and(|at| at > std::time::Instant::now())
                {
                    t.reconnected.to_string()
                } else {
                    match (waiting_on_model, state.live_reasoning.is_empty()) {
                        (true, false) => t.thinking.to_string(),
                        (true, true) => t.waiting_model.to_string(),
                        (false, _) => state
                            .activity
                            .clone()
                            .unwrap_or_else(|| t.waiting_model.to_string()),
                    }
                }
            }
        }
    };
    let turn_mode = if state.goal_mode_active {
        format!("{} · ", t.goal_mode)
    } else {
        String::new()
    };
    // An activity that owns a clock — a long command's heartbeat — reports its
    // own elapsed. Appending the turn's as well printed two adjacent durations
    // with nothing to tell them apart.
    let mut parts = vec![format!("{frame} {turn_mode}{label}")];
    // Where in the plan, when there IS a plan. A turn with no plan gets no
    // fraction: "步骤 1/1" invented to look purposeful is chrome, not progress.
    if let Some(step) = live_plan_step_chip(state, t) {
        parts.push(step);
    }
    parts.push(fmt_elapsed(
        state.activity_elapsed_secs.unwrap_or(state.elapsed_secs),
    ));
    // How much work the turn has actually done. Zero is not reported — nothing
    // has happened, and a "0 次工具" reads as a measurement of nothing.
    if state.turn_tool_calls > 0 {
        parts.push(
            t.tool_calls_n
                .replacen("{}", &state.turn_tool_calls.to_string(), 1)
                .trim_start_matches([' ', '\u{b7}'])
                .trim()
                .to_string(),
        );
    }
    // Totals are only reported when a round ENDS, so on their own they
    // freeze for the whole of the next round. Show them, then always
    // append the live estimate for the round in flight — that is the
    // only number that moves while the model is thinking.
    if state.token_input > 0 || state.token_output > 0 {
        parts.push(format!(
            "↑{} ↓{}",
            fmt_tokens(state.token_input),
            fmt_tokens(state.token_output)
        ));
    }
    if let Some(est) = streaming_output_estimate(state) {
        parts.push(format!("↓~{}", fmt_tokens(est)));
    }
    let text = fit_status(&parts, width);
    let rest = text.strip_prefix(frame).unwrap_or(&text);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            frame.to_string(),
            Style::default()
                .fg(theme.accent.primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(rest.to_string(), Style::default().fg(theme.text.secondary)),
    ])];
    append_activity_rows(&mut lines, state, width, theme);
    lines
}

fn blocked_wait_lines(
    state: &AppState,
    view: &crate::wait_status::WaitView,
    width: usize,
) -> Vec<Line<'static>> {
    let theme = &state.theme;
    let t = state.t();
    let turn_mode = if state.goal_mode_active {
        format!("{} · ", t.goal_mode)
    } else {
        String::new()
    };
    let headline = crate::wait_status::headline(view, t);
    let text = truncate_to_width(&format!("{WAIT_MARKER} {turn_mode}{headline}"), width);
    let rest = text.strip_prefix(WAIT_MARKER).unwrap_or(&text);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            WAIT_MARKER.to_string(),
            Style::default().fg(theme.accent.primary),
        ),
        Span::styled(rest.to_string(), Style::default().fg(theme.text.secondary)),
    ])];
    append_activity_rows(&mut lines, state, width, theme);
    lines
}

fn append_activity_rows(
    lines: &mut Vec<Line<'static>>,
    state: &AppState,
    width: usize,
    theme: &crate::theme::Theme,
) {
    for row in crate::activity::status_activity_lines(state, width, state.t()) {
        lines.push(Line::from(Span::styled(
            truncate_to_width(&row.text, width),
            Style::default().fg(if row.selected {
                theme.text.primary
            } else {
                theme.text.muted
            }),
        )));
    }
}

/// Sparse top chrome: `⑂ branch · ~/path` only. Trust signals sit by the
/// prompt (composer bottom-border trust chip).
/// Collapse a repository path's `$HOME` prefix to `~` for display, or `—` when
/// no repo is set. Shared by the header, workbench, and splash surfaces so the
/// collapse rule stays in one place.
pub(crate) fn home_collapsed_repo(state: &AppState) -> String {
    if state.repository.is_empty() {
        return "—".to_string();
    }
    let repo = state.repository.as_str();
    match leveler_core::environment().var_os("HOME") {
        Some(h) => match repo.strip_prefix(h.to_string_lossy().as_ref()) {
            Some(rest) => format!("~{rest}"),
            None => repo.to_string(),
        },
        None => repo.to_string(),
    }
}

pub(crate) fn header_line(state: &AppState, width: usize) -> Line<'static> {
    let theme = &state.theme;
    let branch = state.branch.as_deref().unwrap_or("—");
    let repo_disp = home_collapsed_repo(state);

    let full = format!("⑂ {branch} · {repo_disp}");
    let mid = format!("⑂ {branch}");
    let text = [full.as_str(), mid.as_str(), branch]
        .into_iter()
        .find(|s| UnicodeWidthStr::width(*s) <= width)
        .unwrap_or(branch)
        .to_string();

    Line::from(Span::styled(
        truncate_to_width(&text, width),
        Style::default().fg(theme.text.secondary),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> AppState {
        AppState::new(
            crate::theme::Theme::no_color(),
            crate::state::Boot {
                session_id: leveler_client_protocol::SessionId::new("s1"),
                user: "u".into(),
                version: "0".into(),
                show_welcome: false,
                draft_path: None,
                history_path: None,
                context_window: 0,
                locale: crate::i18n::Locale::Zh,
                untrusted_config: Vec::new(),
                reasoning_effort: None,
            },
        )
    }

    // ── §8: the live activity line, and only real facts on it ──────────────

    fn busy_line(state: &AppState) -> String {
        status_lines(state, 140)
            .first()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|sp| sp.content.as_ref())
                    .collect::<String>()
            })
            .unwrap_or_default()
    }

    fn plan_at(current: usize, total: usize) -> leveler_client_protocol::UiPlan {
        leveler_client_protocol::UiPlan {
            steps: (0..total)
                .map(|i| leveler_client_protocol::UiPlanStep {
                    index: i,
                    description: format!("step {}", i + 1),
                    status: match i.cmp(&current) {
                        std::cmp::Ordering::Less => leveler_client_protocol::PlanStepStatus::Done,
                        std::cmp::Ordering::Equal => {
                            leveler_client_protocol::PlanStepStatus::Running
                        }
                        std::cmp::Ordering::Greater => {
                            leveler_client_protocol::PlanStepStatus::Pending
                        }
                    },
                })
                .collect(),
        }
    }

    /// A1: with a real plan and real tool calls, the live line says where in
    /// the plan the turn is and how much work it has done.
    #[test]
    fn the_live_line_reports_the_plan_step_and_the_tool_count() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.activity = Some("正在检查 WorkerConfig".into());
        state.plan = Some(plan_at(1, 4));
        state.turn_tool_calls = 3;
        state.elapsed_secs = 12;
        let line = busy_line(&state);
        assert!(line.contains("正在检查 WorkerConfig"), "{line}");
        assert!(
            line.contains("第 2/4 步进行中"),
            "the step the plan declares in progress, 1-based: {line}"
        );
        assert!(!line.contains("当前"), "no runtime cursor: {line}");
        assert!(line.contains("3 次工具"), "{line}");
        assert!(line.contains("12s"), "{line}");
    }

    /// A2: no plan, no step counter. A turn with no plan must not be given
    /// "步骤 1/1" to look busy.
    #[test]
    fn without_a_plan_the_live_line_claims_no_step() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.activity = Some("正在分析".into());
        state.turn_tool_calls = 2;
        let line = busy_line(&state);
        assert!(!line.contains("步骤"), "{line}");
        assert!(!line.contains('/'), "no invented fraction: {line}");
        assert!(line.contains("2 次工具"), "{line}");
    }

    /// A3: nothing has run yet, so nothing is counted. A "0 次工具" would be
    /// chrome pretending to be a measurement.
    #[test]
    fn a_turn_with_no_tool_calls_yet_shows_no_count() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.activity = Some("正在思考".into());
        assert!(
            !busy_line(&state).contains("次工具"),
            "{}",
            busy_line(&state)
        );
    }

    #[test]
    fn token_estimate_scales_with_text() {
        assert_eq!(estimate_tokens(""), 0);
        let short = estimate_tokens("介绍一下这个项目");
        let long = estimate_tokens(&"介绍一下这个项目".repeat(10));
        assert!(long > short);
        assert_eq!(estimate_tokens("abcd"), 1);
    }

    #[test]
    fn header_is_branch_and_path_only() {
        let mut state = test_state();
        state.branch = Some("kcn".into());
        state.repository = "/Users/example/projects/example-service".into();
        state.model_label = "deepseek/v3".into();
        state.mode_label = "Assisted".into();
        let header = header_line(&state, 120).to_string();
        assert!(header.contains("kcn"), "{header}");
        assert!(
            header.contains("example-service") || header.contains("projects"),
            "{header}"
        );
        assert!(
            !header.contains("deepseek") && !header.contains("替我审批"),
            "model/perm belong near prompt, not header: {header}"
        );
    }

    #[test]
    fn compact_token_and_footer_ctx_chip() {
        assert_eq!(fmt_tokens_compact(41181), "41k");
        assert_eq!(fmt_tokens_compact(1_048_576), "1M");
        assert_eq!(fmt_tokens_compact(900), "900");
        let mut state = test_state();
        state.context_tokens = 41_181;
        state.context_window_tokens = 1_048_576;
        assert_eq!(footer_ctx_chip(&state).as_deref(), Some("上下文 41k/1M"));
        assert_eq!(footer_cache_chip(&state), None);
        assert_eq!(footer_usage_line(&state).as_deref(), Some("上下文 41k/1M"));
        state.token_input = 1000;
        state.token_cached = 420;
        assert_eq!(footer_cache_chip(&state).as_deref(), Some("缓存 42%"));
        assert_eq!(
            footer_usage_line(&state).as_deref(),
            Some("上下文 41k/1M · 缓存 42%")
        );
    }

    #[test]
    fn clock_formats_as_24h_hh_mm_without_seconds() {
        let at = |h, m, s| chrono::NaiveTime::from_hms_opt(h, m, s).unwrap();
        assert_eq!(fmt_clock(at(0, 5, 0)), "00:05");
        assert_eq!(fmt_clock(at(9, 7, 0)), "09:07");
        assert_eq!(fmt_clock(at(22, 22, 59)), "22:22");
        assert_eq!(fmt_clock(at(23, 59, 0)), "23:59");
    }

    /// The clock is no longer glued to the usage chips: the row's owner places
    /// the clock as its own far-right cell. Usage is just the chips.
    #[test]
    fn footer_usage_line_reports_the_runtime_chips_only() {
        let mut state = test_state();
        state.context_tokens = 254_000;
        state.context_window_tokens = 1_048_576;
        state.token_input = 1000;
        state.token_cached = 990;
        state.clock_label = "22:22".into();
        assert_eq!(
            footer_usage_line(&state).as_deref(),
            Some("上下文 254k/1M · 缓存 99%")
        );
    }

    /// A session with no usage has no usage row at all; the clock is placed
    /// independently by `workbench::render_footer`.
    #[test]
    fn footer_usage_line_is_hidden_without_usage() {
        let mut state = test_state();
        state.clock_label = "09:07".into();
        assert_eq!(footer_usage_line(&state), None);
    }

    #[test]
    fn goal_mode_is_visible_in_status() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.goal_mode_active = true;
        let status = status_line_content(&state, 120).to_string();
        assert!(status.contains("目标模式"), "status: {status}");
        assert!(status.contains("等待模型"), "status: {status}");
    }

    /// Reasoning tokens arriving is not waiting. A real turn sat on
    /// "等待模型 · 12m 00s" while `↓~35,348` climbed beside it — twelve minutes
    /// of a word that says nothing is happening, with the only evidence to the
    /// contrary being a raw token counter. The status line already refuses a
    /// bare "等待模型" whenever a more specific stage is known; the model
    /// streaming its reasoning is one.
    #[test]
    fn streaming_reasoning_is_not_reported_as_waiting() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.live_reasoning = "先确认环境，再决定怎么落盘……".repeat(20);
        let status = status_line_content(&state, 120).to_string();
        assert!(status.contains("思考"), "status: {status}");
        assert!(!status.contains("等待模型"), "status: {status}");
    }

    /// A running command owns the status line. Reasoning text left over from
    /// before it started must not displace its heartbeat.
    #[test]
    fn a_running_command_still_outranks_streaming_reasoning() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.live_reasoning = "……".repeat(40);
        state.activity = Some("运行 cargo test".into());
        let status = status_line_content(&state, 120).to_string();
        assert!(status.contains("运行 cargo test"), "status: {status}");
    }

    /// Before a single reasoning token arrives, waiting is exactly what it is.
    #[test]
    fn an_idle_model_call_still_reads_as_waiting() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        let status = status_line_content(&state, 120).to_string();
        assert!(status.contains("等待模型"), "status: {status}");
    }

    #[test]
    fn finalizing_status_names_the_runtime_stage_not_the_model() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.finalization_stage = Some(FinalizationStage::Verification);
        let status = status_line_content(&state, 120).to_string();
        assert!(status.contains("正在验证"), "status: {status}");
        assert!(!status.contains("等待模型"), "status: {status}");
    }

    #[test]
    fn header_prefers_branch_when_narrow() {
        let mut state = test_state();
        state.branch = Some("main".into());
        state.repository = "/very/long/path/to/a/repository".into();
        let narrow = header_line(&state, 12).to_string();
        assert!(narrow.contains("main") || narrow.contains("⑂"), "{narrow}");
    }

    #[test]
    fn permission_chip_labels_are_english_product_terms() {
        let mut state = test_state();
        state.mode_label = "Assisted".into();
        assert_eq!(permission_chip_label(&state), "auto");
        state.mode_label = "RequestApproval".into();
        assert_eq!(permission_chip_label(&state), "ask");
        state.mode_label = "FullAccess".into();
        assert_eq!(permission_chip_label(&state), "full");
    }

    #[test]
    fn runtime_chip_strips_provider_and_shows_effort() {
        let mut state = test_state();
        state.model_label = "deepseek/deepseek-v4-flash".into();
        state.reasoning_effort = Some("max".into());
        state.work_profile = "balanced".into();
        state.mode_label = "RequestApproval".into();
        state.collaboration = "chat".into();
        assert_eq!(
            runtime_status_chip(&state, 80),
            "deepseek-v4-flash (max) · balanced · ask · chat"
        );
    }

    #[test]
    fn runtime_chip_omits_effort_when_unset() {
        let mut state = test_state();
        state.model_label = "deepseek/deepseek-v4-flash".into();
        state.reasoning_effort = None;
        state.work_profile = "balanced".into();
        state.mode_label = "RequestApproval".into();
        state.collaboration = "chat".into();
        let chip = runtime_status_chip(&state, 80);
        assert_eq!(chip, "deepseek-v4-flash · balanced · ask · chat");
        assert!(!chip.contains('('), "{chip}");
    }

    #[test]
    fn runtime_chip_keeps_provider_when_model_names_collide() {
        let mut state = test_state();
        state.model_label = "openai/gpt-5".into();
        state.available_models = vec![
            leveler_client_protocol::ModelRef::parse("openai/gpt-5").unwrap(),
            leveler_client_protocol::ModelRef::parse("azure/gpt-5").unwrap(),
        ];
        state.work_profile = "balanced".into();
        state.mode_label = "Assisted".into();
        state.collaboration = "chat".into();
        assert!(
            runtime_status_chip(&state, 80).starts_with("openai/gpt-5"),
            "{}",
            runtime_status_chip(&state, 80)
        );
    }

    #[test]
    fn runtime_chip_drops_low_priority_fields_when_narrow() {
        let mut state = test_state();
        state.model_label = "deepseek/deepseek-v4-flash".into();
        state.reasoning_effort = Some("max".into());
        state.work_profile = "balanced".into();
        state.mode_label = "RequestApproval".into();
        state.collaboration = "chat".into();
        let mid = runtime_status_chip(&state, 36);
        assert!(mid.contains("deepseek-v4-flash"), "{mid}");
        assert!(mid.contains("balanced"), "{mid}");
        assert!(!mid.contains("chat"), "session is dropped first: {mid}");
        let tight = runtime_status_chip(&state, 22);
        assert!(tight.contains("deepseek-v4-flash"), "{tight}");
        assert!(!tight.contains("balanced"), "{tight}");
    }

    #[test]
    fn status_phase_idle_when_nothing_running() {
        let state = test_state();
        assert_eq!(status_phase(&state), StatusPhase::Idle);
        let text = status_line_content(&state, 120).to_string();
        assert!(
            text.is_empty() || !SPINNER.iter().any(|s| text.contains(s)),
            "idle must not look busy: {text}"
        );
    }

    #[test]
    fn status_phase_busy_shows_spinner_activity() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.activity = Some("running tools".into());
        state.tick = 0;
        assert_eq!(status_phase(&state), StatusPhase::Busy);
        let text = status_line_content(&state, 120).to_string();
        assert!(
            text.contains(SPINNER[0]) && text.contains("running tools"),
            "busy status: {text}"
        );
    }

    #[test]
    fn approval_overlay_is_awaiting_user_without_spinner_even_if_busy() {
        use leveler_client_protocol::{ApprovalId, UiApprovalRequest};
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.activity = Some("should not show as running".into());
        state.tick = 3;
        state.overlay = Some(crate::overlay::Overlay::Approval(Box::new(
            crate::overlay::ApprovalOverlay::new(UiApprovalRequest {
                id: ApprovalId::new("a1"),
                tool: "run_command".into(),
                summary: "git push".into(),
                command: Some("git push".into()),
                risks: vec!["network".into()],
                call_id: None,
                always_persists: true,
            }),
        )));
        assert_eq!(status_phase(&state), StatusPhase::AwaitingUser);
        let text = status_line_content(&state, 120).to_string();
        assert!(
            text.contains(state.t().overlay_approval)
                || text.contains("授权")
                || text.contains("approval"),
            "awaiting-user copy: {text}"
        );
        for frame in SPINNER {
            assert!(
                !text.contains(frame),
                "must not paint busy spinner while awaiting approval: {text}"
            );
        }
        assert!(
            !text.contains("should not show as running"),
            "must not leak busy activity under approval: {text}"
        );
    }

    #[test]
    fn clarification_overlay_is_awaiting_user() {
        use leveler_client_protocol::{ClarificationId, UiClarificationRequest};
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.overlay = Some(crate::overlay::Overlay::Clarification(Box::new(
            crate::overlay::ClarificationOverlay::new(UiClarificationRequest::single(
                ClarificationId::new("c1"),
                "which branch?",
                vec!["main".into(), "dev".into()],
            )),
        )));
        assert_eq!(status_phase(&state), StatusPhase::AwaitingUser);
        let text = status_line_content(&state, 120).to_string();
        assert!(
            text.contains(state.t().waiting_reply)
                || text.contains("等待")
                || text.contains("waiting"),
            "clarification wait copy: {text}"
        );
        for frame in SPINNER {
            assert!(!text.contains(frame), "no spinner: {text}");
        }
    }

    /// A reasoning model can spend 90+ seconds and thousands of tokens on a
    /// single round while emitting almost no visible text (measured: 95s,
    /// 6738 output tokens, 45 characters of answer). During that round the
    /// status line kept showing the PREVIOUS round's totals, so every number
    /// on screen was frozen and the turn looked hung. Live rounds must show
    /// live progress.
    #[test]
    fn streaming_reasoning_shows_live_progress_not_stale_totals() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        // Previous round reported usage; without the fix this alone wins.
        state.token_input = 51_360;
        state.token_output = 633;
        // Current round is streaming reasoning and nothing else yet.
        state.live_reasoning = "思考".repeat(400);
        let status = status_line_content(&state, 160).to_string();
        assert!(
            status.contains('~'),
            "a live round must show a live estimate, not only frozen totals: {status}"
        );
    }

    /// A command's duration is the command's. The turn clock belongs to an
    /// activity that has no clock of its own, and a command is not one: its
    /// heartbeat — and the row right above — already count it separately.
    ///
    /// R1 dogfood, 2026-09-16: `node --version` ran for 0.12s at turn elapsed
    /// 294s. Its own row read "运行中 · 0s" while the status line, in the same
    /// frame, read "执行命令 node --version · 4m 54s" — the turn's number on
    /// the command's label.
    #[test]
    fn a_command_is_never_given_the_turn_clock() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.elapsed_secs = 294;
        crate::reducer::reduce(
            &mut state,
            crate::action::Action::Runtime(
                leveler_client_protocol::RuntimeEvent::ToolCallStarted {
                    id: leveler_client_protocol::ToolCallId::new("c1"),
                    name: "run_command".into(),
                    arguments: r#"{"program":"node","args":["--version"]}"#.into(),
                    parallel: false,
                },
            ),
        );
        let text = status_text(&state);
        assert!(text.contains("执行命令 node --version"), "{text}");
        assert!(
            !text.contains("4m 54s"),
            "the turn's clock is not the command's: {text}"
        );
    }

    /// The other side of the same rule: a tool that has no clock of its own —
    /// a read — still reports the turn's, because that IS its only clock.
    #[test]
    fn a_read_still_reports_the_turn_clock() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.elapsed_secs = 294;
        crate::reducer::reduce(
            &mut state,
            crate::action::Action::Runtime(
                leveler_client_protocol::RuntimeEvent::ToolCallStarted {
                    id: leveler_client_protocol::ToolCallId::new("r1"),
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                    parallel: false,
                },
            ),
        );
        let text = status_text(&state);
        assert!(text.contains("读取文件 a.rs"), "{text}");
        assert!(text.contains("4m 54s"), "{text}");
    }

    /// Beta Product Closure, Phase B. A running command already carries its own
    /// elapsed in the activity label; appending the turn's elapsed printed two
    /// adjacent durations with nothing to tell them apart —
    /// "运行 cargo test --workspace · 2m 17s · 0s".
    #[test]
    fn a_running_command_shows_one_elapsed_not_two() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.elapsed_secs = 0;
        crate::reducer::reduce(
            &mut state,
            crate::action::Action::Runtime(
                leveler_client_protocol::RuntimeEvent::CommandProgress {
                    label: "cargo test --workspace".into(),
                    elapsed_ms: 137_000,
                },
            ),
        );
        let text = status_text(&state);
        let durations = text
            .split(" · ")
            .filter(|p| {
                p.ends_with('s') && p.chars().next().is_some_and(|c| c.is_ascii_digit())
                    || p.contains("m ") && p.ends_with('s')
            })
            .count();
        assert_eq!(
            durations, 1,
            "one running command, one elapsed — got {durations} in: {text}"
        );
        assert!(
            text.contains("2m 17s"),
            "and it must be the command's: {text}"
        );
    }

    fn status_text(state: &AppState) -> String {
        status_lines(state, 120)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A turn blocked on `wait_task` is main execution state: the strip names
    /// the wait. The task itself no longer occupies a strip row — its summary
    /// lives in the input footer and its detail is reachable from the jobs
    /// list.
    #[test]
    fn status_strip_names_a_background_wait_without_a_task_row() {
        use crate::state::BackgroundTaskChrome;
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.elapsed_secs = 54;
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 10),
        );
        state.transcript.push_tool_started(
            leveler_client_protocol::ToolCallId::new("w1"),
            "wait_task".into(),
            serde_json::json!({ "task_id": "bg-2" }).to_string(),
            false,
            12,
        );
        state.activity = Some("等待任务".into());
        let text = status_text(&state);
        assert!(text.contains("等待后台任务"), "{text}");
        assert!(
            !text.contains("cargo test --workspace"),
            "the task row left the strip: {text}"
        );
        assert!(!text.contains('↗'), "the row affordance left too: {text}");
        assert!(text.contains(WAIT_MARKER), "{text}");
        for line in text.lines() {
            if line.contains("等待任务") {
                assert!(line.contains("等待后台任务"), "{text}");
            }
        }
        assert!(!text.contains("可能卡住"), "{text}");
        assert!(!text.contains("最近活动"), "{text}");
    }

    #[test]
    fn status_strip_does_not_treat_background_as_a_main_wait() {
        use crate::state::BackgroundTaskChrome;
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.activity = Some("正在汇总审计结果".into());
        state.background_task_labels.insert(
            "bg-1".into(),
            BackgroundTaskChrome::running("cargo test --workspace", 0),
        );
        let text = status_text(&state);
        assert!(text.contains("正在汇总审计结果"), "{text}");
        assert!(
            !text.contains("cargo test --workspace"),
            "a running background task never occupies a strip row: {text}"
        );
        assert!(!text.contains("等待后台任务"), "{text}");
    }

    /// A background process outlives its turn. It stays reachable while Main is
    /// idle through the input footer's aggregated summary, not a strip row.
    #[test]
    fn an_idle_strip_keeps_background_out_of_the_strip_but_in_the_footer() {
        use crate::state::BackgroundTaskChrome;
        let mut state = test_state();
        state.status = RuntimeStatus::Idle;
        state.activity = None;
        state.elapsed_secs = 90;
        state.background_task_labels.insert(
            "bg-1".into(),
            BackgroundTaskChrome::running("npm run dev", 0),
        );
        state.background_task_labels.insert(
            "bg-2".into(),
            BackgroundTaskChrome {
                label: "cargo test --workspace".into(),
                started_elapsed_secs: 0,
                ok: Some(true),
                stopped: false,
                exit_code: Some(0),
                duration_ms: Some(1_000),
                output: String::new(),
            },
        );
        let text = status_text(&state);
        assert!(!text.contains("npm run dev"), "{text}");
        assert!(!text.contains("cargo test --workspace"), "{text}");
        assert!(!text.contains('↗'), "{text}");
        assert!(!text.contains("等待后台任务"), "{text}");
        // Still reachable: the footer summary reports the running task.
        let summary = crate::activity::footer_summary(&state).expect("footer summary");
        assert_eq!(summary.running, 1);
        assert_eq!(summary.single_label.as_deref(), Some("npm run dev"));
    }

    /// A live reconnect is the one fact that must survive the busy status line:
    /// the generic "waiting for model" would hide it. The trailing number is
    /// the runtime's OWN announced delay, counted down from it.
    #[test]
    fn a_reconnect_owns_the_status_line_over_the_model_wait() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.reconnecting = Some(crate::state::Reconnecting {
            attempt: 2,
            max_attempts: 10,
            delay: std::time::Duration::from_secs(4),
            retry_at: std::time::Instant::now() + std::time::Duration::from_secs(4),
        });
        let text = status_text(&state);
        assert!(text.contains("正在重连 · 2/10 · "), "{text}");
        assert!(text.contains('s'), "the countdown is shown: {text}");
        assert!(!text.contains("等待模型"), "{text}");
    }

    /// The brief "Reconnected" confirmation owns the line after a retry
    /// attempt starts again, then yields to the normal streaming status.
    #[test]
    fn a_brief_reconnected_confirmation_yields_to_streaming() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.reconnected_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
        assert!(status_text(&state).contains("已重连"));

        // Once the confirmation window passes, the line is the normal pending
        // model status — a stale "Reconnected" must not linger.
        state.reconnected_until =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        assert!(!status_text(&state).contains("已重连"));
    }
}
