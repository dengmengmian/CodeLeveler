//! Status and bottom-bar rendering with sparse chrome.
//!
//! Layout contract (matches the product screenshot target):
//!
//! - **Header** (1 line): branch · repo path (muted; identity only)
//! - **Status** (1 line): live activity only (empty when idle; toasts float)
//! - **Input border**: `{model} [(effort)] · work-mode · permission · session`
//! - **Footer** (1 line): runtime context only — `Context 8k/1M`
//!
//! Vertical breathing (workbench): blank above the input when status/queue/plan
//! chrome is visible; blank between input and the Context footer always.
//!
//! Shortcuts are not sticky chrome — discover via `/help` or `Ctrl+?`.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use leveler_client_protocol::RuntimeStatus;

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
    let thinking = estimate_tokens(&state.reasoning);
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
    Some(format!(
        "Context {}/{}",
        fmt_tokens_compact(used),
        fmt_tokens_compact(window)
    ))
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
    Some(format!("cache {pct}%"))
}

/// Full footer status: `Context 21k/1M · cache 42%` — each part optional.
pub(crate) fn footer_status_line(state: &AppState) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(ctx) = footer_ctx_chip(state) {
        parts.push(ctx);
    }
    if let Some(cache) = footer_cache_chip(state) {
        parts.push(cache);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
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

pub(crate) fn status_line_content(state: &AppState, width: usize) -> Line<'static> {
    let theme = &state.theme;
    // Phase first: never paint a busy spinner while blocked on the user.
    match status_phase(state) {
        StatusPhase::AwaitingUser => {
            if let Some(overlay) = &state.overlay {
                if matches!(overlay, crate::overlay::Overlay::Clarification(_)) {
                    return turn_marker(
                        state.t().waiting_reply.to_string(),
                        theme.accent.primary,
                        width,
                        state,
                    );
                }
                // Interrupting overlays: static waiting copy, no spinner.
                // Pickers return None and fall through to the normal strip.
                if let Some(hint) = overlay.status_hint(state.t()) {
                    return Line::from(Span::styled(
                        hint,
                        Style::default().fg(theme.status.warning),
                    ));
                }
            }
        }
        StatusPhase::Busy | StatusPhase::Idle => {}
    }
    // Notifications are floating toasts in the workbench; never put them in
    // the layout status strip (that reflows Conversation under a selection).
    match state.status {
        RuntimeStatus::Busy => {
            let frame = SPINNER[(state.tick as usize) % SPINNER.len()];
            let t = state.t();
            let label = state
                .activity
                .clone()
                .unwrap_or_else(|| t.waiting_model.to_string());
            let turn_mode = if state.goal_mode_active {
                format!("{} · ", t.goal_mode)
            } else {
                String::new()
            };
            let mut parts = vec![
                format!("{frame} {turn_mode}{label}"),
                fmt_elapsed(state.elapsed_secs),
            ];
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
            // Give the moving spinner glyph an accent color so "still working"
            // reads at a glance; keep the rest of the line low-key.
            let rest = text.strip_prefix(frame).unwrap_or(&text);
            Line::from(vec![
                Span::styled(
                    frame.to_string(),
                    Style::default()
                        .fg(theme.accent.primary)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(rest.to_string(), Style::default().fg(theme.text.secondary)),
            ])
        }
        RuntimeStatus::Error | RuntimeStatus::Idle => {
            if let Some(label) = &state.activity {
                Line::from(Span::styled(
                    format!("… {label}"),
                    Style::default().fg(theme.text.secondary),
                ))
            } else {
                Line::from("")
            }
        }
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
        assert_eq!(footer_ctx_chip(&state).as_deref(), Some("Context 41k/1M"));
        assert_eq!(footer_cache_chip(&state), None);
        assert_eq!(
            footer_status_line(&state).as_deref(),
            Some("Context 41k/1M")
        );
        state.token_input = 1000;
        state.token_cached = 420;
        assert_eq!(footer_cache_chip(&state).as_deref(), Some("cache 42%"));
        assert_eq!(
            footer_status_line(&state).as_deref(),
            Some("Context 41k/1M · cache 42%")
        );
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
            crate::overlay::ClarificationOverlay::new(UiClarificationRequest {
                id: ClarificationId::new("c1"),
                question: "which branch?".into(),
                options: vec!["main".into(), "dev".into()],
            }),
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
        state.reasoning = "思考".repeat(400);
        let status = status_line_content(&state, 160).to_string();
        assert!(
            status.contains('~'),
            "a live round must show a live estimate, not only frozen totals: {status}"
        );
    }
}
