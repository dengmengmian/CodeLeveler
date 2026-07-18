//! The permission approval overlay .
//!
//! Safe by default in two ways, both required by the spec: the initial focus is
//! **Deny** (never the allow row), and dismissing the overlay (Esc / Ctrl+C)
//! resolves to **Deny**, never to an approval. Letter shortcuts
//! (`y` / `s` / `w` / `d`) give quick answers.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use leveler_client_protocol::{ApprovalDecision, UiApprovalRequest};

/// The four decisions, ordered with the safe option last so the default
/// cursor (Deny) sits on it.
const OPTIONS: [(&str, ApprovalDecision); 4] = [
    ("仅允许本次", ApprovalDecision::ApproveOnce),
    ("本次会话内允许", ApprovalDecision::ApproveSession),
    // Edits: whole apply_patch/replace tool. Shell: program [arg] prefix.
    (
        "始终允许（写入项目规则，同类不再问）",
        ApprovalDecision::ApproveAlways,
    ),
    ("拒绝", ApprovalDecision::Deny),
];

const DENY_INDEX: usize = 3;

/// Result of a key press on the approval overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// Consumed; stay open.
    None,
    /// The user decided; resolve the pending request.
    Decide(ApprovalDecision),
}

/// The approval overlay state.
#[derive(Debug, Clone)]
pub struct ApprovalOverlay {
    pub request: UiApprovalRequest,
    cursor: usize,
}

impl ApprovalOverlay {
    /// Open the overlay with the cursor on the safe (Deny) option.
    pub fn new(request: UiApprovalRequest) -> Self {
        Self {
            request,
            cursor: DENY_INDEX,
        }
    }

    /// Rows for rendering: `(label, is_cursor)`.
    pub fn options(&self) -> Vec<(&'static str, bool)> {
        OPTIONS
            .iter()
            .enumerate()
            .map(|(i, (label, _))| (*label, i == self.cursor))
            .collect()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ApprovalOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ApprovalOutcome::None;
        }
        match key.code {
            // Dismissal always resolves to the safe decision.
            KeyCode::Esc => ApprovalOutcome::Decide(ApprovalDecision::Deny),
            KeyCode::Char('y') => ApprovalOutcome::Decide(ApprovalDecision::ApproveOnce),
            // `a` kept for muscle memory; prompt prefers `s`.
            KeyCode::Char('a') | KeyCode::Char('s') => {
                ApprovalOutcome::Decide(ApprovalDecision::ApproveSession)
            }
            KeyCode::Char('w') => ApprovalOutcome::Decide(ApprovalDecision::ApproveAlways),
            KeyCode::Char('d') | KeyCode::Char('n') => {
                ApprovalOutcome::Decide(ApprovalDecision::Deny)
            }
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                ApprovalOutcome::None
            }
            KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(OPTIONS.len() - 1);
                ApprovalOutcome::None
            }
            KeyCode::Enter => ApprovalOutcome::Decide(OPTIONS[self.cursor].1),
            _ => ApprovalOutcome::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::ApprovalId;

    fn request() -> UiApprovalRequest {
        UiApprovalRequest {
            id: ApprovalId::new("r1"),
            tool: "run_command".into(),
            summary: "run git push".into(),
            command: Some("git push".into()),
            risks: vec!["将访问网络".into()],
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn default_focus_is_deny() {
        let ov = ApprovalOverlay::new(request());
        let focused = ov.options().into_iter().find(|(_, f)| *f).unwrap();
        assert_eq!(focused.0, "拒绝");
    }

    #[test]
    fn enter_on_default_denies() {
        let mut ov = ApprovalOverlay::new(request());
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ApprovalOutcome::Decide(ApprovalDecision::Deny)
        );
    }

    #[test]
    fn esc_denies_never_approves() {
        let mut ov = ApprovalOverlay::new(request());
        assert_eq!(
            ov.on_key(key(KeyCode::Esc)),
            ApprovalOutcome::Decide(ApprovalDecision::Deny)
        );
    }

    #[test]
    fn letter_shortcuts_decide() {
        let mut ov = ApprovalOverlay::new(request());
        assert_eq!(
            ov.on_key(key(KeyCode::Char('y'))),
            ApprovalOutcome::Decide(ApprovalDecision::ApproveOnce)
        );
        assert_eq!(
            ov.on_key(key(KeyCode::Char('a'))),
            ApprovalOutcome::Decide(ApprovalDecision::ApproveSession)
        );
        assert_eq!(
            ov.on_key(key(KeyCode::Char('s'))),
            ApprovalOutcome::Decide(ApprovalDecision::ApproveSession)
        );
        assert_eq!(
            ov.on_key(key(KeyCode::Char('w'))),
            ApprovalOutcome::Decide(ApprovalDecision::ApproveAlways)
        );
    }

    #[test]
    fn arrow_up_then_enter_approves_always() {
        let mut ov = ApprovalOverlay::new(request());
        ov.on_key(key(KeyCode::Up)); // Deny(3) -> Always(2)
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ApprovalOutcome::Decide(ApprovalDecision::ApproveAlways)
        );
    }

    #[test]
    fn arrow_up_thrice_then_enter_approves_once() {
        let mut ov = ApprovalOverlay::new(request());
        ov.on_key(key(KeyCode::Up)); // Deny -> Always
        ov.on_key(key(KeyCode::Up)); // Always -> Session
        ov.on_key(key(KeyCode::Up)); // Session -> Once
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ApprovalOutcome::Decide(ApprovalDecision::ApproveOnce)
        );
    }
}
