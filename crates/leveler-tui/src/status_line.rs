//! Status and bottom-bar rendering with sparse chrome.
//!
//! Layout contract (matches the product screenshot target):
//!
//! - **Header** (1 line): branch · repo path (muted; identity only)
//! - **Status** (1 line): live activity only (empty when idle; toasts float)
//! - **Input border**: model · permission (`auto` / `approve` / `full`)
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
/// Stable English permission chip for the Input border (`model · auto`).
///
/// Keeps the product surface short and language-neutral next to the model id.
pub(crate) fn permission_chip_label(state: &AppState) -> &'static str {
    match state.mode_label.as_str() {
        "RequestApproval" => "approve",
        "FullAccess" => "full",
        _ => "auto",
    }
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

/// Visual context usage: bar + percent when window is known.
/// Used by tests and the legacy bottom-bar helper; the workbench footer uses a
/// simpler `Context used/window` form.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn context_progress(state: &AppState) -> Option<(String, f32)> {
    let used = state.context_tokens;
    let window = state.context_window();
    if used == 0 && window == 0 {
        return None;
    }
    let t = state.t();
    if window == 0 {
        return Some((
            t.context_tokens_only.replacen("{}", &fmt_tokens(used), 1),
            0.0,
        ));
    }
    if used == 0 {
        return Some((
            t.context_empty_window
                .replacen("{}", &fmt_tokens(window), 1),
            0.0,
        ));
    }
    let pct = (used as f32 / window as f32 * 100.0).min(999.0);
    let filled = ((pct / 10.0).round() as usize).min(10);
    let bar = format!(
        "[{}{}] {:.0}%",
        "█".repeat(filled),
        "░".repeat(10usize.saturating_sub(filled)),
        pct
    );
    let s = t
        .context_with_bar
        .replacen("{}", &bar, 1)
        .replacen("{}", &fmt_tokens(used), 1)
        .replacen("{}", &fmt_tokens(window), 1);
    Some((s, pct))
}

#[cfg(test)]
fn token_context_gauge(state: &AppState) -> String {
    let Some((ctx, pct)) = context_progress(state) else {
        return String::new();
    };
    let t = state.t();
    let mut parts = Vec::new();
    if state.token_input > 0 || state.token_output > 0 {
        let cached = if state.token_cached > 0 && state.token_input > 0 {
            let cache_pct = (state.token_cached as u64 * 100) / state.token_input as u64;
            t.cached_pct.replacen("{}", &cache_pct.to_string(), 1)
        } else {
            String::new()
        };
        parts.push(format!(
            "↑ {}{cached} · ↓ {}",
            fmt_tokens(state.token_input),
            fmt_tokens(state.token_output)
        ));
    }
    parts.push(ctx);
    if pct >= 90.0 {
        parts.push(t.suggest_compact.to_string());
    } else if pct >= 70.0 {
        parts.push(t.compact_hint.to_string());
    }
    parts.join(" · ")
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

#[cfg(test)]
fn truncate_styled_spans(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::from("");
    }
    let total: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if total <= width {
        return Line::from(spans);
    }
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let w = UnicodeWidthStr::width(span.content.as_ref());
        if used + w <= width {
            used += w;
            out.push(span);
            continue;
        }
        let room = width.saturating_sub(used);
        if room > 1 {
            let cut = truncate_to_width(span.content.as_ref(), room);
            out.push(Span::styled(cut, span.style));
        }
        break;
    }
    Line::from(out)
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
                        theme.accent,
                        width,
                        state,
                    );
                }
                // Approval and other decision overlays: static waiting copy, no spinner.
                return Line::from(Span::styled(
                    overlay.status_hint(state.t()),
                    Style::default().fg(theme.warning),
                ));
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
            let waiting = state.input_queues.waiting_len();
            if waiting > 0 {
                parts.push(t.pending_n.replacen("{}", &waiting.to_string(), 1));
            }
            let text = fit_status(&parts, width);
            // Give the moving spinner glyph an accent color so "still working"
            // reads at a glance; keep the rest of the line low-key.
            let rest = text.strip_prefix(frame).unwrap_or(&text);
            Line::from(vec![
                Span::styled(
                    frame.to_string(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(rest.to_string(), Style::default().fg(theme.muted)),
            ])
        }
        RuntimeStatus::Error | RuntimeStatus::Idle => {
            if let Some(label) = &state.activity {
                Line::from(Span::styled(
                    format!("… {label}"),
                    Style::default().fg(theme.muted),
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
        Style::default().fg(theme.muted),
    ))
}

/// Legacy trust-strip helper (tests). Production paints model · permission on
/// the composer bottom border via `composer_trust_chip`.
#[cfg(test)]
pub(crate) fn bottom_bar_lines(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let theme = &state.theme;
    let perm = permission_chip_label(state);
    let muted = Style::default().fg(theme.muted);
    let model = Style::default().fg(theme.text).add_modifier(Modifier::BOLD);
    let sep = Style::default().fg(theme.border);

    let mut spans = vec![
        Span::styled(state.model_label.clone(), model),
        Span::styled(" · ", sep),
        Span::styled(perm.to_string(), muted),
    ];
    // Idle empty tip once — points people at the help card, not a key laundry list.
    if !state.is_busy()
        && state.composer.is_empty()
        && state.notification.is_none()
        && state.token_input == 0
        && state.context_tokens == 0
    {
        spans.push(Span::styled(" · ", sep));
        spans.push(Span::styled("/help", muted));
    }

    vec![truncate_styled_spans(spans, width)]
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
    fn gauge_reports_prefix_cache_hit_rate() {
        let mut state = test_state();
        state.context_tokens = 1000;
        state.token_input = 1000;
        state.token_output = 50;
        state.token_cached = 900;
        let gauge = token_context_gauge(&state);
        assert!(
            gauge.contains("90%") && (gauge.contains("缓存") || gauge.contains("cached")),
            "gauge: {gauge}"
        );
    }

    #[test]
    fn gauge_omits_cache_when_provider_reports_none() {
        let mut state = test_state();
        state.context_tokens = 1000;
        state.token_input = 1000;
        state.token_output = 50;
        state.token_cached = 0;
        let gauge = token_context_gauge(&state);
        assert!(
            !gauge.contains("缓存") && !gauge.contains("cached"),
            "gauge: {gauge}"
        );
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
    fn bottom_bar_is_model_perm_not_token_spam() {
        let mut state = test_state();
        state.model_label = "deepseek/v3".into();
        state.mode_label = "Assisted".into();
        state.context_tokens = 8_000;
        state.context_window_tokens = 100_000;
        let text: String = bottom_bar_lines(&state, 120)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("deepseek/v3"), "{text}");
        assert!(text.contains("auto"), "{text}");
        assert!(
            !text.contains("↑")
                && !text.contains("Context ")
                && !text.contains("Ctrl+O")
                && !text.contains("⇧↑")
                && !text.contains("ctrl+j")
                && !text.contains("ctrl+c")
                && !text.contains("发送"),
            "chip is model · permission only: {text}"
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
    fn context_progress_shows_bar() {
        let mut state = test_state();
        state.context_tokens = 8000;
        state.context_window_tokens = 10000;
        let (label, pct) = context_progress(&state).unwrap();
        assert!((pct - 80.0).abs() < 0.1);
        assert!(label.contains('%'));
    }

    #[test]
    fn high_context_suggests_compact() {
        let mut state = test_state();
        state.context_tokens = 95_000;
        state.context_window_tokens = 100_000;
        let gauge = token_context_gauge(&state);
        assert!(gauge.contains("/compact"), "gauge: {gauge}");
    }

    #[test]
    fn known_window_shows_before_first_usage() {
        let mut state = test_state();
        state.context_window_tokens = 128_000;
        let gauge = token_context_gauge(&state);
        assert!(gauge.contains("128,000"), "gauge: {gauge}");
        assert!(!gauge.contains('%'), "no fake usage bar: {gauge}");
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
    fn busy_status_counts_only_waiting_queue_items() {
        let mut state = test_state();
        state.status = RuntimeStatus::Busy;
        state.input_queues.pending = vec!["sending now".into()];
        let sending = status_line_content(&state, 120).to_string();
        assert!(
            !sending.contains("待处理"),
            "pending item is already sending: {sending}"
        );
        state.input_queues.queued = vec!["next".into()];
        let waiting = status_line_content(&state, 120).to_string();
        assert!(waiting.contains("+1 待处理"), "waiting item: {waiting}");
    }

    #[test]
    fn permission_chip_labels_are_english_product_terms() {
        let mut state = test_state();
        state.mode_label = "Assisted".into();
        assert_eq!(permission_chip_label(&state), "auto");
        state.mode_label = "RequestApproval".into();
        assert_eq!(permission_chip_label(&state), "approve");
        state.mode_label = "FullAccess".into();
        assert_eq!(permission_chip_label(&state), "full");
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

    #[test]
    fn bottom_bar_renders_english_permission_chip() {
        let mut state = test_state();
        state.mode_label = "Assisted".into();
        state.model_label = "m".into();
        let text: String = bottom_bar_lines(&state, 80)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("m · auto") || text.contains("auto"), "{text}");
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
