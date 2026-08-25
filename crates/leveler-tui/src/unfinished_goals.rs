//! Read-only presentation of goals that still owe work (long-goal P2).
//!
//! Three rules, all of them about not overclaiming:
//!
//! 1. **No action.** No Resume, no Continue, no Restart. The runtime has not
//!    decided a resume policy, and a button the runtime cannot honestly back
//!    is worse than none.
//! 2. **No verdict.** Not "failed", not "interrupted" — nothing issued either.
//!    The truthful statement is that work is owed and nobody is doing it.
//! 3. **No claim on another runtime's work.** A foreign-owned goal is listed,
//!    because omitting work that exists is worse, but never as *your* problem.

use leveler_client_protocol::UiUnfinishedGoal;

/// One rendered row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRow {
    pub objective: String,
    /// What is true about it. Never a verdict.
    pub status: String,
    /// Windows already spent, when any were.
    pub spent: Option<String>,
    /// The user can act on this one.
    pub actionable: bool,
}

/// Should the panel appear at all? An empty list is not a section header.
pub fn should_show(goals: &[UiUnfinishedGoal]) -> bool {
    !goals.is_empty()
}

/// Rows for the unfinished-goal view, most recent first (the runtime already
/// orders them).
pub fn goal_rows(goals: &[UiUnfinishedGoal], t: &crate::i18n::UiText) -> Vec<GoalRow> {
    goals
        .iter()
        .map(|g| {
            let status = if g.driving {
                // Being driven right now: it owes work, but not from the user.
                t.goal_running_now
            } else if !g.ours {
                // Another runtime holds it. Naming it is honest; asking this
                // user to do something about it is not.
                t.goal_other_runtime
            } else {
                t.goal_needs_attention
            };
            GoalRow {
                objective: g.objective.clone(),
                status: status.to_string(),
                spent: (g.windows_run > 0).then(|| {
                    t.goal_windows_spent
                        .replace("{n}", &g.windows_run.to_string())
                }),
                actionable: g.needs_attention(),
            }
        })
        .collect()
}

/// Panel title. Counts here because "how much is owed" is the whole point of
/// the view — unlike the team panel, where the count of agents says nothing.
pub fn title(goals: &[UiUnfinishedGoal], t: &crate::i18n::UiText) -> String {
    let n = goals.iter().filter(|g| g.needs_attention()).count();
    if n == 0 {
        return t.goals_none_need_attention.to_string();
    }
    t.goals_title.replace("{n}", &n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(objective: &str, ours: bool, driving: bool, windows: u32) -> UiUnfinishedGoal {
        UiUnfinishedGoal {
            goal_id: "g1".into(),
            objective: objective.into(),
            session_id: "s1".into(),
            opened_at: "2026-08-26T00:00:00Z".into(),
            windows_run: windows,
            ours,
            driving,
        }
    }

    #[test]
    fn an_empty_list_is_not_a_section() {
        assert!(!should_show(&[]));
    }

    #[test]
    fn owed_actionable_work_is_the_only_thing_called_needing_attention() {
        let t = crate::i18n::Locale::En.text();
        let rows = goal_rows(&[goal("auth refactor", true, false, 3)], t);
        assert_eq!(rows[0].objective, "auth refactor");
        assert!(rows[0].actionable);
        assert!(rows[0].status.contains("attention"), "{:?}", rows[0]);
        assert_eq!(rows[0].spent.as_deref(), Some("3 windows"));
    }

    /// The wording rule. Nothing issued a verdict, so the UI must not.
    #[test]
    fn the_wording_is_never_a_verdict() {
        let t = crate::i18n::Locale::En.text();
        for g in [
            goal("a", true, false, 0),
            goal("b", false, false, 1),
            goal("c", true, true, 2),
        ] {
            let rows = goal_rows(&[g], t);
            let s = rows[0].status.to_lowercase();
            assert!(!s.contains("fail"), "{s}");
            assert!(!s.contains("interrupt"), "{s}");
            assert!(!s.contains("crash"), "{s}");
            assert!(!s.contains("error"), "{s}");
        }
    }

    /// No row may offer an action, because there is no action to offer.
    #[test]
    fn no_row_offers_to_continue_anything() {
        let t = crate::i18n::Locale::En.text();
        let rows = goal_rows(&[goal("a", true, false, 1)], t);
        let s = rows[0].status.to_lowercase();
        for word in ["resume", "continue", "restart", "retry"] {
            assert!(!s.contains(word), "the UI must not offer {word}: {s}");
        }
    }

    #[test]
    fn a_goal_being_driven_is_not_the_users_problem() {
        let t = crate::i18n::Locale::En.text();
        let rows = goal_rows(&[goal("live", true, true, 1)], t);
        assert!(!rows[0].actionable, "somebody is already doing it");
    }

    #[test]
    fn a_foreign_owned_goal_is_listed_but_not_claimed() {
        let t = crate::i18n::Locale::En.text();
        let rows = goal_rows(&[goal("theirs", false, false, 0)], t);
        assert_eq!(rows.len(), 1, "listed: omitting real work is worse");
        assert!(!rows[0].actionable, "not this user's to act on");
    }

    #[test]
    fn the_title_counts_only_what_the_user_can_act_on() {
        let t = crate::i18n::Locale::En.text();
        let goals = vec![
            goal("mine", true, false, 0),
            goal("theirs", false, false, 0),
            goal("live", true, true, 0),
        ];
        assert!(title(&goals, t).contains('1'), "{}", title(&goals, t));
    }

    #[test]
    fn nothing_owed_says_so_rather_than_showing_a_zero() {
        let t = crate::i18n::Locale::En.text();
        let title = title(&[goal("live", true, true, 0)], t);
        assert!(!title.contains('0'), "{title}");
    }
}
