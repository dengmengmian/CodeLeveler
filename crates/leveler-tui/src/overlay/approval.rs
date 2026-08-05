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
    // Persisted as a project rule: whole tool (apply_patch), a `program [arg]`
    // prefix (simple shell), or the exact command (compound shell). "不再问"
    // is honest for all three; the scope varies by command shape.
    (
        "始终允许（写入项目规则，以后不再问）",
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
    /// Show the command in full instead of elided to one line.
    expanded: bool,
}

impl ApprovalOverlay {
    /// Open the overlay with the cursor on the safe (Deny) option.
    pub fn new(request: UiApprovalRequest) -> Self {
        Self {
            request,
            cursor: DENY_INDEX,
            expanded: false,
        }
    }

    pub fn expanded(&self) -> bool {
        self.expanded
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
            // The headline elides the command to keep the prompt one line;
            // Ctrl+O is how you read the rest before deciding on it.
            if matches!(key.code, KeyCode::Char('o')) {
                self.expanded = !self.expanded;
            }
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
            // Numbered rows are the fastest path when the prompt reads as a
            // question with answers rather than a dialog to arrow through.
            KeyCode::Char(c @ '1'..='9') => match c.to_digit(10).map(|d| d as usize - 1) {
                Some(i) if i < OPTIONS.len() => ApprovalOutcome::Decide(OPTIONS[i].1),
                _ => ApprovalOutcome::None,
            },
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
    fn ctrl_o_toggles_the_full_command() {
        // The headline elides so the prompt stays one line, which means there
        // has to be a way to read the rest before approving it.
        let mut ov = ApprovalOverlay::new(request());
        assert!(!ov.expanded());
        assert_eq!(
            ov.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            ApprovalOutcome::None
        );
        assert!(ov.expanded());
        ov.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(!ov.expanded());
    }

    #[test]
    fn other_ctrl_keys_still_decide_nothing() {
        let mut ov = ApprovalOverlay::new(request());
        assert_eq!(
            ov.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            ApprovalOutcome::None
        );
        assert!(!ov.expanded());
    }

    #[test]
    fn number_keys_pick_the_matching_option() {
        let mut ov = ApprovalOverlay::new(request());
        assert_eq!(
            ov.on_key(key(KeyCode::Char('1'))),
            ApprovalOutcome::Decide(ApprovalDecision::ApproveOnce)
        );
        assert_eq!(
            ov.on_key(key(KeyCode::Char('4'))),
            ApprovalOutcome::Decide(ApprovalDecision::Deny)
        );
        // Out of range is inert, never a stray decision.
        assert_eq!(ov.on_key(key(KeyCode::Char('9'))), ApprovalOutcome::None);
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
