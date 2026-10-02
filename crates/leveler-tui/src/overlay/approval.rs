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
///
/// The decision is the constant; its wording is not. A permission prompt is
/// the one surface where a reader who cannot read the label may still press a
/// key, so the label is resolved from the active locale at render time rather
/// than frozen into this table.
const OPTIONS: [ApprovalDecision; 4] = [
    ApprovalDecision::ApproveOnce,
    // The runtime keeps this grant for the rest of the current turn; there is
    // no grant that outlives it.
    ApprovalDecision::ApproveSession,
    // Persisted as a project rule: whole tool (apply_patch), a `program [arg]`
    // prefix (simple shell), or the exact command (compound shell). "never ask
    // again" is honest for all three; the scope varies by command shape.
    ApprovalDecision::ApproveAlways,
    ApprovalDecision::Deny,
];

/// The user-facing wording for one decision.
pub(crate) fn decision_label(decision: ApprovalDecision, t: &crate::i18n::UiText) -> &'static str {
    match decision {
        ApprovalDecision::ApproveOnce => t.approval_once,
        ApprovalDecision::ApproveSession => t.approval_session,
        ApprovalDecision::ApproveProject => t.approval_project,
        ApprovalDecision::ApproveAlways => t.approval_always,
        ApprovalDecision::Deny => t.approval_deny,
    }
}

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
    /// The call this decision holds, parsed once from the request. Lets the
    /// transcript row for that call say "waiting on you" instead of "running".
    pub(crate) gated_call: Option<leveler_client_protocol::ToolCallId>,
    cursor: usize,
    /// Show the command in full instead of elided to one line.
    expanded: bool,
}

impl ApprovalOverlay {
    /// Open the overlay with the cursor on the safe (Deny) option.
    pub fn new(request: UiApprovalRequest) -> Self {
        let cursor = choices(&request).len() - 1;
        let gated_call = request
            .call_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(leveler_client_protocol::ToolCallId::new);
        Self {
            request,
            gated_call,
            cursor,
            expanded: false,
        }
    }

    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// Rows for rendering: `(label, is_cursor)`.
    pub fn options(&self, t: &crate::i18n::UiText) -> Vec<(&'static str, bool)> {
        choices(&self.request)
            .iter()
            .enumerate()
            .map(|(i, decision)| {
                (
                    if *decision == ApprovalDecision::ApproveSession && self.request.grant.is_some()
                    {
                        t.approval_resource_session
                    } else {
                        decision_label(*decision, t)
                    },
                    i == self.cursor,
                )
            })
            .collect()
    }

    pub fn on_key(&mut self, key: KeyEvent) -> ApprovalOutcome {
        let choices = choices(&self.request);
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
            KeyCode::Char('s') if choices.contains(&ApprovalDecision::ApproveSession) => {
                ApprovalOutcome::Decide(ApprovalDecision::ApproveSession)
            }
            KeyCode::Char('a')
                if self.request.grant.is_none()
                    && choices.contains(&ApprovalDecision::ApproveSession) =>
            {
                ApprovalOutcome::Decide(ApprovalDecision::ApproveSession)
            }
            KeyCode::Char('p') if choices.contains(&ApprovalDecision::ApproveProject) => {
                ApprovalOutcome::Decide(ApprovalDecision::ApproveProject)
            }
            KeyCode::Char('w') if choices.contains(&ApprovalDecision::ApproveAlways) => {
                ApprovalOutcome::Decide(ApprovalDecision::ApproveAlways)
            }
            KeyCode::Char('d') | KeyCode::Char('n') => {
                ApprovalOutcome::Decide(ApprovalDecision::Deny)
            }
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                ApprovalOutcome::None
            }
            KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(choices.len() - 1);
                ApprovalOutcome::None
            }
            // Numbered rows are the fastest path when the prompt reads as a
            // question with answers rather than a dialog to arrow through.
            KeyCode::Char(c @ '1'..='9') => match c.to_digit(10).map(|d| d as usize - 1) {
                Some(i) if i < choices.len() => ApprovalOutcome::Decide(choices[i]),
                _ => ApprovalOutcome::None,
            },
            KeyCode::Enter => ApprovalOutcome::Decide(choices[self.cursor]),
            _ => ApprovalOutcome::None,
        }
    }
}

/// The rows this request offers. "Always" is shown only when the runtime
/// would persist a rule for it; otherwise it would promise what never happens.
fn choices(request: &UiApprovalRequest) -> Vec<ApprovalDecision> {
    if request.requires_human_consent {
        return vec![ApprovalDecision::ApproveOnce, ApprovalDecision::Deny];
    }
    if request.project_available() {
        return vec![
            ApprovalDecision::ApproveOnce,
            ApprovalDecision::ApproveSession,
            ApprovalDecision::ApproveProject,
            ApprovalDecision::Deny,
        ];
    }
    OPTIONS
        .into_iter()
        .filter(|decision| {
            *decision != ApprovalDecision::ApproveAlways
                || (request.grant.is_none() && request.always_persists)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::ApprovalId;

    #[test]
    fn resource_bound_choices_offer_project_and_session_and_human_consent_does_not() {
        let mut req = request();
        req.grant = Some(leveler_core::GrantRequest {
            project_identity: "project-a".into(),
            bindings: vec![leveler_core::GrantBinding {
                capability: leveler_core::Capability::RemoteMutate,
                resource: leveler_core::ResourceIdentity::ConfiguredRemote {
                    repository: "repo-a".into(),
                    remote_name: "origin".into(),
                    canonical_url: "https://example.test/repo.git".into(),
                    transport: "https".into(),
                },
            }],
        });
        assert_eq!(
            choices(&req),
            vec![
                ApprovalDecision::ApproveOnce,
                ApprovalDecision::ApproveSession,
                ApprovalDecision::ApproveProject,
                ApprovalDecision::Deny
            ]
        );
        let mut overlay = ApprovalOverlay::new(req.clone());
        assert_eq!(
            overlay.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            ApprovalOutcome::Decide(ApprovalDecision::ApproveProject)
        );
        req.requires_human_consent = true;
        assert_eq!(
            choices(&req),
            vec![ApprovalDecision::ApproveOnce, ApprovalDecision::Deny]
        );
        let mut overlay = ApprovalOverlay::new(req);
        assert_eq!(
            overlay.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
            ApprovalOutcome::None
        );
        assert_eq!(
            overlay.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            ApprovalOutcome::None
        );
    }

    fn request() -> UiApprovalRequest {
        UiApprovalRequest {
            grant: None,
            requires_human_consent: false,
            id: ApprovalId::new("r1"),
            tool: "run_command".into(),
            summary: "run git push".into(),
            command: Some("git push".into()),
            risks: vec!["将访问网络".into()],
            call_id: None,
            always_persists: true,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    /// A consent tool (`save_agent`, `remember`, …) cannot become a project
    /// rule, so the overlay must not offer one: no row, no `w` shortcut, and
    /// the numbers follow the rows that are shown.
    #[test]
    fn always_is_not_offered_when_no_rule_would_be_written() {
        let mut ov = ApprovalOverlay::new(UiApprovalRequest {
            tool: "save_agent".into(),
            always_persists: false,
            ..request()
        });
        let labels: Vec<&str> = ov
            .options(crate::i18n::Locale::Zh.text())
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        assert_eq!(labels, vec!["仅允许本次", "本轮对话内允许", "拒绝"]);
        assert_eq!(
            ov.options(crate::i18n::Locale::Zh.text())
                .into_iter()
                .find(|(_, f)| *f)
                .unwrap()
                .0,
            "拒绝"
        );
        assert_eq!(ov.on_key(key(KeyCode::Char('w'))), ApprovalOutcome::None);
        assert_eq!(
            ov.on_key(key(KeyCode::Char('3'))),
            ApprovalOutcome::Decide(ApprovalDecision::Deny)
        );
        assert_eq!(ov.on_key(key(KeyCode::Char('4'))), ApprovalOutcome::None);
        ov.on_key(key(KeyCode::Up));
        assert_eq!(
            ov.on_key(key(KeyCode::Enter)),
            ApprovalOutcome::Decide(ApprovalDecision::ApproveSession)
        );
    }

    /// The rows read as product choices and never as the mechanism behind them:
    /// the persisted row names its scope, and the legacy rule it writes is not
    /// mentioned anywhere.
    #[test]
    fn every_scope_is_named_in_product_words() {
        for (grant, expected) in [
            (
                None,
                vec!["仅允许本次", "本轮对话内允许", "此项目内始终允许", "拒绝"],
            ),
            (
                Some(leveler_core::GrantRequest {
                    project_identity: "project-a".into(),
                    bindings: vec![leveler_core::GrantBinding {
                        capability: leveler_core::Capability::RemoteMutate,
                        resource: leveler_core::ResourceIdentity::ConfiguredRemote {
                            repository: "repo-a".into(),
                            remote_name: "origin".into(),
                            canonical_url: "https://example.test/a.git".into(),
                            transport: "https".into(),
                        },
                    }],
                }),
                vec!["仅允许本次", "本次会话内允许", "此项目内始终允许", "拒绝"],
            ),
        ] {
            let mut req = request();
            req.always_persists = true;
            req.grant = grant;
            let ov = ApprovalOverlay::new(req);
            let labels: Vec<&str> = ov
                .options(crate::i18n::Locale::Zh.text())
                .into_iter()
                .map(|(label, _)| label)
                .collect();
            assert_eq!(labels, expected);
            for label in labels {
                for internal in ["旧项目规则", "legacy", "grant", "capability", "resource"] {
                    assert!(
                        !label.to_lowercase().contains(internal),
                        "{internal} is internal wording: {label}"
                    );
                }
            }
        }
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
        let focused = ov
            .options(crate::i18n::Locale::Zh.text())
            .into_iter()
            .find(|(_, f)| *f)
            .unwrap();
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
