//! `/memory`: the management page, the discovery notice and the consent
//! batch. The store is the authority — these tests pin that the client asks
//! for its listing, never mutates it, and never reports an unproven outcome.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    ClientCommand, CommandId, ModelRef, PermissionProfile, RuntimeEvent, SessionId,
    UiMemoryCandidate, UiMemoryEntry, UiSessionSnapshot,
};

use super::reduce;
use crate::action::{Action, Effect};
use crate::memory::MemoryView;
use crate::screen::Screen;
use crate::state::{AppState, Boot};

fn state() -> AppState {
    let mut s = AppState::new(
        crate::theme::Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: crate::i18n::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    );
    s.size = (80, 24);
    s
}

fn snapshot(status: &str) -> UiSessionSnapshot {
    UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: Some("/work/demo".into()),
        task_status: None,
        task_terminal: None,
        goal: "g".into(),
        model: ModelRef::parse("deepseek/v3"),
        mode: PermissionProfile::Assisted,
        branch: None,
        status: status.into(),
        finalization_stage: None,
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        active_background_tasks: Vec::new(),
        plan: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        thinking: None,
        work_profile: None,
        collaboration: None,
        execution_task: None,
        children: Vec::new(),
    }
}

fn candidate(id: &str, title: &str) -> UiMemoryCandidate {
    UiMemoryCandidate {
        id: id.into(),
        scope: leveler_client_protocol::UiMemoryScope::Project,
        title: title.into(),
        body: title.into(),
        kind: "free".into(),
        source: "user_explicit".into(),
    }
}

fn candidate_in(
    scope: leveler_client_protocol::UiMemoryScope,
    id: &str,
    title: &str,
) -> UiMemoryCandidate {
    UiMemoryCandidate {
        scope,
        ..candidate(id, title)
    }
}

fn entry(id: &str, title: &str) -> UiMemoryEntry {
    UiMemoryEntry {
        id: id.into(),
        scope: leveler_client_protocol::UiMemoryScope::Project,
        title: title.into(),
        body: title.into(),
        kind: None,
        sensitive: false,
        status: Some("active".into()),
        authority: Some("user_explicit".into()),
        revision: Some(1),
        updated_at: Some("2026-01-01T00:00:00Z".into()),
        pinned: false,
    }
}

fn list(
    query: Option<CommandId>,
    active: Vec<UiMemoryEntry>,
    archived: Vec<UiMemoryEntry>,
    pending: Vec<UiMemoryCandidate>,
) -> RuntimeEvent {
    RuntimeEvent::MemoryList {
        query_id: query,
        memory_dir: "/state/memory".into(),
        active,
        archived,
        pending,
    }
}

/// The listing query a set of effects asks the runtime for.
fn list_query(effects: &[Effect]) -> Option<CommandId> {
    effects.iter().find_map(|effect| match effect {
        Effect::Send(ClientCommand::ListMemory { query_id, .. }) => query_id.clone(),
        _ => None,
    })
}

fn sent_accepts(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(ClientCommand::AcceptMemory { id, .. }) => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn sent_rejects(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(ClientCommand::RejectMemory { id, .. }) => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn key(s: &mut AppState, code: KeyCode) -> Vec<Effect> {
    reduce(s, Action::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn press(s: &mut AppState, command: &str) -> Vec<Effect> {
    s.composer.replace(command);
    key(s, KeyCode::Enter)
}

/// Open the session and answer its discovery probe with `pending`.
fn seed(s: &mut AppState, pending: Vec<UiMemoryCandidate>) -> Vec<Effect> {
    let effects = reduce(
        s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("idle"),
        }),
    );
    let query = list_query(&effects).expect("a session open must discover durable pending");
    reduce(
        s,
        Action::Runtime(list(Some(query), vec![], vec![], pending)),
    )
}

fn open_review(s: &mut AppState) {
    press(s, "/memory review");
    assert_eq!(s.active_screen, Screen::Memory);
    assert_eq!(s.memory.view, MemoryView::Review);
}

#[test]
fn opening_a_session_asks_for_durable_pending() {
    let mut s = state();
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("idle"),
        }),
    );
    assert!(
        list_query(&effects).is_some(),
        "without a probe, candidates proposed while this client was away stay invisible: {effects:?}"
    );
    assert!(
        s.memory.probe_query.is_some(),
        "the probe must be owned so another client's listing cannot announce"
    );
}

#[test]
fn first_discovery_announces_the_pending_count() {
    let mut s = state();
    seed(
        &mut s,
        (0..12)
            .map(|i| candidate(&format!("cand-{i}"), &format!("约定 {i}")))
            .collect(),
    );
    let note = s
        .notification
        .as_ref()
        .expect("twelve pending candidates must not stay silent");
    assert!(note.message.contains("12"), "{}", note.message);
    assert!(note.message.contains("/memory review"), "{}", note.message);
    assert!(note.message.contains("尚未生效"), "{}", note.message);
}

#[test]
fn the_same_batch_is_not_announced_twice() {
    let mut s = state();
    let pending: Vec<_> = (0..3)
        .map(|i| candidate(&format!("cand-{i}"), &format!("约定 {i}")))
        .collect();
    seed(&mut s, pending.clone());
    s.notification = None;

    // A reconnect or restart re-probes the same durable set.
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("idle"),
        }),
    );
    let query = list_query(&effects).expect("the resumed session probes again");
    reduce(
        &mut s,
        Action::Runtime(list(Some(query), vec![], vec![], pending)),
    );
    assert!(
        s.notification.is_none(),
        "the same batch was announced again: {:?}",
        s.notification
    );
}

#[test]
fn newly_discovered_candidates_announce_only_the_new_count() {
    let mut s = state();
    let mut pending: Vec<_> = (0..3)
        .map(|i| candidate(&format!("cand-{i}"), &format!("约定 {i}")))
        .collect();
    seed(&mut s, pending.clone());
    s.notification = None;

    pending.push(candidate("cand-new", "新的约定"));
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("idle"),
        }),
    );
    let query = list_query(&effects).expect("probe");
    reduce(
        &mut s,
        Action::Runtime(list(Some(query), vec![], vec![], pending)),
    );
    let note = s.notification.as_ref().expect("a new candidate is news");
    assert!(note.message.contains("1 条"), "{}", note.message);
    assert!(!note.message.contains("4 条"), "{}", note.message);
}

#[test]
fn a_running_turn_defers_the_notice_until_it_ends() {
    let mut s = state();
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("running"),
        }),
    );
    let query = list_query(&effects).expect("probe");
    reduce(
        &mut s,
        Action::Runtime(list(
            Some(query),
            vec![],
            vec![],
            vec![candidate("cand-1", "约定")],
        )),
    );
    assert!(
        s.notification.is_none(),
        "discovery must not compete with a running task"
    );
    assert_eq!(s.memory.notice_pending, Some(1));

    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    let note = s
        .notification
        .as_ref()
        .expect("the notice surfaces at the turn boundary");
    assert!(note.message.contains("/memory review"), "{}", note.message);
}

#[test]
fn memory_slash_opens_the_management_page() {
    let mut s = state();
    seed(&mut s, vec![candidate("cand-1", "约定")]);
    s.notification = None;

    let effects = press(&mut s, "/memory");
    assert_eq!(s.active_screen, Screen::Memory);
    assert_eq!(s.memory.view, MemoryView::Home);
    assert!(list_query(&effects).is_some(), "{effects:?}");

    key(&mut s, KeyCode::Esc);
    assert_eq!(s.active_screen, Screen::Conversation);
    press(&mut s, "/memory review");
    assert_eq!(s.memory.view, MemoryView::Review);
}

#[test]
fn the_home_menu_opens_remembered_then_pending_then_archived() {
    let mut s = state();
    seed(&mut s, vec![candidate("cand-1", "约定")]);
    let effects = press(&mut s, "/memory");
    let query = list_query(&effects).expect("opening the page lists memory");
    reduce(
        &mut s,
        Action::Runtime(list(
            Some(query),
            vec![entry("act-1", "已记住")],
            vec![entry("arch-1", "已归档")],
            vec![candidate("cand-1", "待确认")],
        )),
    );

    key(&mut s, KeyCode::Enter);
    assert_eq!(s.memory.view, MemoryView::Active);
    key(&mut s, KeyCode::Esc);
    key(&mut s, KeyCode::Down);
    key(&mut s, KeyCode::Enter);
    assert_eq!(s.memory.view, MemoryView::Review);
    key(&mut s, KeyCode::Esc);
    key(&mut s, KeyCode::Down);
    key(&mut s, KeyCode::Enter);
    assert_eq!(s.memory.view, MemoryView::Archived);
}

#[test]
fn review_selection_follows_record_ids_across_a_reorder() {
    let mut s = state();
    seed(
        &mut s,
        vec![
            candidate("cand-a", "A"),
            candidate("cand-b", "B"),
            candidate("cand-c", "C"),
        ],
    );
    open_review(&mut s);
    assert_eq!(s.memory.focused_id(), Some("project:cand-a"));

    key(&mut s, KeyCode::Char(' ')); // select A
    key(&mut s, KeyCode::Down);
    key(&mut s, KeyCode::Char(' ')); // select B
    assert_eq!(s.memory.selected.len(), 2);

    // The backend reorders the same records; selection is by id, not index.
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("idle"),
        }),
    );
    let query = list_query(&effects).expect("probe");
    reduce(
        &mut s,
        Action::Runtime(list(
            Some(query),
            vec![],
            vec![],
            vec![
                candidate("cand-c", "C"),
                candidate("cand-b", "B"),
                candidate("cand-a", "A"),
            ],
        )),
    );
    assert_eq!(
        s.memory.selected.iter().cloned().collect::<Vec<_>>(),
        vec!["project:cand-a", "project:cand-b"]
    );

    let effects = key(&mut s, KeyCode::Char('a'));
    let mut accepted = sent_accepts(&effects);
    accepted.sort();
    assert_eq!(accepted, vec!["cand-a", "cand-b"]);
}

#[test]
fn accepting_selected_records_sends_one_command_each_and_refuses_a_second_batch() {
    let mut s = state();
    seed(
        &mut s,
        vec![
            candidate("cand-a", "A"),
            candidate("cand-b", "B"),
            candidate("cand-c", "C"),
        ],
    );
    open_review(&mut s);
    key(&mut s, KeyCode::Char(' '));
    key(&mut s, KeyCode::Down);
    key(&mut s, KeyCode::Char(' '));

    let effects = key(&mut s, KeyCode::Char('a'));
    assert_eq!(sent_accepts(&effects), vec!["cand-a", "cand-b"]);
    assert!(sent_rejects(&effects).is_empty());
    assert!(s.memory.busy());

    let again = key(&mut s, KeyCode::Char('a'));
    assert!(
        sent_accepts(&again).is_empty(),
        "a second keypress must not submit the same consent again: {again:?}"
    );
}

/// The same record id in two stores is two records. The decision must carry
/// the scope of the row the user actually consented to.
#[test]
fn the_same_record_id_in_two_scopes_is_two_rows() {
    use leveler_client_protocol::UiMemoryScope;

    let mut s = state();
    seed(
        &mut s,
        vec![
            candidate_in(UiMemoryScope::Global, "same-id", "全局约定"),
            candidate_in(UiMemoryScope::Project, "same-id", "项目约定"),
        ],
    );
    open_review(&mut s);
    assert_eq!(s.memory.focused_id(), Some("global:same-id"));

    key(&mut s, KeyCode::Char(' '));
    let effects = key(&mut s, KeyCode::Char('a'));
    let accepted: Vec<(UiMemoryScope, String)> = effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(ClientCommand::AcceptMemory { scope, id, .. }) => {
                Some((*scope, id.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(accepted, vec![(UiMemoryScope::Global, "same-id".into())]);
    assert!(
        s.memory
            .pending
            .iter()
            .any(|c| c.scope == UiMemoryScope::Project && c.id == "same-id"),
        "the project record was not decided"
    );
}

#[test]
fn escape_walks_back_one_level_and_never_confirms() {
    let mut s = state();
    seed(&mut s, vec![candidate("cand-a", "A")]);
    open_review(&mut s);
    key(&mut s, KeyCode::Char(' '));

    let effects = key(&mut s, KeyCode::Esc);
    assert!(sent_accepts(&effects).is_empty());
    assert!(sent_rejects(&effects).is_empty());
    assert_eq!(s.memory.view, MemoryView::Home);
    assert!(
        s.memory.selected.contains("project:cand-a"),
        "Esc is not a decision"
    );

    let effects = key(&mut s, KeyCode::Esc);
    assert!(sent_accepts(&effects).is_empty());
    assert_eq!(s.active_screen, Screen::Conversation);
}

#[test]
fn the_detail_page_returns_to_the_list_that_opened_it() {
    let mut s = state();
    seed(&mut s, vec![candidate("cand-a", "A")]);
    open_review(&mut s);
    key(&mut s, KeyCode::Enter);
    assert_eq!(s.memory.view, MemoryView::Detail);
    key(&mut s, KeyCode::Esc);
    assert_eq!(s.memory.view, MemoryView::Review);
}

#[test]
fn a_partial_batch_is_reported_per_record() {
    let mut s = state();
    seed(
        &mut s,
        vec![candidate("cand-a", "标题甲"), candidate("cand-b", "标题乙")],
    );
    open_review(&mut s);
    key(&mut s, KeyCode::Char(' '));
    key(&mut s, KeyCode::Down);
    key(&mut s, KeyCode::Char(' '));
    key(&mut s, KeyCode::Char('a'));

    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "已采纳记忆 [cand-a]: 标题甲".into(),
        }),
    );
    assert!(
        list_query(&effects).is_none(),
        "one receipt cannot prove the batch: {effects:?}"
    );
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Warning,
            message: "accept failed [cand-b]: disk full".into(),
        }),
    );
    let query = list_query(&effects).expect("the proving refresh after every receipt");
    reduce(
        &mut s,
        Action::Runtime(list(
            Some(query),
            vec![entry("cand-a", "标题甲")],
            vec![],
            vec![candidate("cand-b", "标题乙")],
        )),
    );

    let report = s.memory.batch.as_ref().expect("a batch report");
    assert_eq!(report.succeeded, vec!["标题甲"]);
    assert_eq!(report.failed, vec!["标题乙"]);
    assert!(report.unconfirmed.is_empty());
    assert!(!s.memory.busy(), "the batch is settled");
    assert!(!s.memory.selected.contains("cand-a"));
}

#[test]
fn a_silent_runtime_is_not_reported_as_success() {
    let mut s = state();
    seed(&mut s, vec![candidate("cand-a", "A")]);
    open_review(&mut s);
    key(&mut s, KeyCode::Char('a'));

    let effects = reduce(
        &mut s,
        Action::IdleTick(Instant::now() + Duration::from_secs(6)),
    );
    let query = list_query(&effects).expect("a timed-out batch asks for the truth");
    reduce(
        &mut s,
        Action::Runtime(list(
            Some(query),
            vec![],
            vec![],
            vec![candidate("cand-a", "A")],
        )),
    );
    let report = s.memory.batch.as_ref().expect("a batch report");
    assert!(report.succeeded.is_empty(), "no proof, no success");
    assert_eq!(report.failed, vec!["A"]);
}

#[test]
fn an_accepted_entry_may_get_a_new_id_and_still_counts_as_success() {
    let mut s = state();
    seed(&mut s, vec![candidate("cand-a", "标题甲")]);
    open_review(&mut s);
    key(&mut s, KeyCode::Char('a'));

    // The store keeps the candidate id only until acceptance; the active
    // entry's own id differs. The request id is what correlates the receipt.
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "已采纳记忆 [cand-a]: 标题甲".into(),
        }),
    );
    let query = list_query(&effects).expect("the proving refresh");
    reduce(
        &mut s,
        Action::Runtime(list(
            Some(query),
            vec![entry("dogfood-a", "标题甲")],
            vec![],
            vec![],
        )),
    );
    let report = s.memory.batch.as_ref().expect("a batch report");
    assert_eq!(report.succeeded, vec!["标题甲"]);
    assert!(
        report.unconfirmed.is_empty(),
        "a durable accept must not be reported as unconfirmed"
    );
}

#[test]
fn a_record_that_vanished_without_a_receipt_is_unconfirmed_not_accepted() {
    let mut s = state();
    seed(&mut s, vec![candidate("cand-a", "标题甲")]);
    open_review(&mut s);
    key(&mut s, KeyCode::Char('a'));

    // There is no receipt; the listing alone must not be read as consent
    // having been applied.
    let effects = reduce(
        &mut s,
        Action::IdleTick(Instant::now() + Duration::from_secs(6)),
    );
    let query = list_query(&effects).expect("a timed-out batch asks for the truth");
    reduce(
        &mut s,
        Action::Runtime(list(
            Some(query),
            vec![entry("dogfood-a", "标题甲")],
            vec![],
            vec![],
        )),
    );
    let report = s.memory.batch.as_ref().expect("a batch report");
    assert!(report.succeeded.is_empty());
    assert_eq!(report.unconfirmed, vec!["标题甲"]);
}

#[test]
fn records_that_left_pending_leave_the_persisted_notified_set() {
    let mut s = state();
    seed(
        &mut s,
        vec![candidate("cand-a", "A"), candidate("cand-b", "B")],
    );
    let first = s
        .memory
        .take_notice_update()
        .expect("the batch was recorded");
    assert_eq!(first.len(), 2);

    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened {
            session: snapshot("idle"),
        }),
    );
    let query = list_query(&effects).expect("probe");
    reduce(
        &mut s,
        Action::Runtime(list(
            Some(query),
            vec![entry("cand-a", "A")],
            vec![],
            vec![candidate("cand-b", "B")],
        )),
    );
    let pruned = s
        .memory
        .take_notice_update()
        .expect("a removed record must also leave the saved file");
    assert_eq!(
        pruned.into_iter().collect::<Vec<_>>(),
        vec!["project:cand-b".to_string()]
    );
}

#[test]
fn notice_state_survives_a_restart_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory-notice.json");
    let ids = ["cand-a".to_string(), "cand-b".to_string()]
        .into_iter()
        .collect();
    crate::memory::save_notified(&path, &ids);
    assert_eq!(crate::memory::load_notified(&path), ids);

    std::fs::write(&path, "not json").unwrap();
    assert!(
        crate::memory::load_notified(&path).is_empty(),
        "a corrupt notice file costs one extra notice, it never blocks the queue"
    );
}

// ----- listing filters ----------------------------------------------------

fn entry_in(
    scope: leveler_client_protocol::UiMemoryScope,
    id: &str,
    title: &str,
    kind: leveler_client_protocol::UiMemoryKind,
) -> UiMemoryEntry {
    UiMemoryEntry {
        id: id.into(),
        scope,
        title: title.into(),
        body: format!("{title} 的正文"),
        kind: Some(kind),
        sensitive: false,
        status: Some("active".into()),
        authority: Some("user_explicit".into()),
        revision: Some(2),
        updated_at: Some("2026-01-02T03:04:05Z".into()),
        pinned: false,
    }
}

/// Open `/memory` on the active list with three rows across scopes and lanes.
fn open_active(s: &mut AppState) {
    let effects = press(s, "/memory");
    // Home's first entry is the active list; the page's own listing query is
    // the one that answers with these rows.
    let query = list_query(&effects).expect("/memory must ask for the listing");
    key(s, KeyCode::Enter);
    let entries = vec![
        entry_in(
            leveler_client_protocol::UiMemoryScope::Project,
            "mem-build",
            "构建使用 cargo",
            leveler_client_protocol::UiMemoryKind::Preference,
        ),
        entry_in(
            leveler_client_protocol::UiMemoryScope::Global,
            "mem-deploy",
            "发布使用 pnpm",
            leveler_client_protocol::UiMemoryKind::Decision,
        ),
        entry_in(
            leveler_client_protocol::UiMemoryScope::Project,
            "mem-exp",
            "cargo 构建失败先清 target",
            leveler_client_protocol::UiMemoryKind::Experience,
        ),
    ];
    reduce(
        s,
        Action::Runtime(list(Some(query), entries, vec![], vec![])),
    );
    assert_eq!(s.memory.view, MemoryView::Active);
}

#[test]
fn a_query_narrows_the_listing_and_keeps_what_it_hides_visible() {
    let mut s = state();
    open_active(&mut s);
    assert_eq!(s.memory.visible_active().len(), 3);

    key(&mut s, KeyCode::Char('/'));
    for character in "pnpm".chars() {
        key(&mut s, KeyCode::Char(character));
    }
    key(&mut s, KeyCode::Enter);

    let visible = s.memory.visible_active();
    assert_eq!(visible.len(), 1, "{visible:?}");
    assert_eq!(visible[0].id, "mem-deploy");
    assert!(
        s.memory.filter.is_narrowing(),
        "the page must be able to say that it is hiding rows"
    );
    // The runtime's listing is untouched: filtering is a view.
    assert_eq!(s.memory.active.len(), 3);

    key(&mut s, KeyCode::Char('x'));
    assert_eq!(s.memory.visible_active().len(), 3);
    assert!(s.memory.filter.query.is_empty());
}

#[test]
fn scope_and_kind_filters_select_rows_without_touching_the_store() {
    let mut s = state();
    open_active(&mut s);

    key(&mut s, KeyCode::Char('s'));
    assert_eq!(
        s.memory.filter.scope,
        Some(leveler_client_protocol::UiMemoryScope::Global)
    );
    assert_eq!(s.memory.visible_active().len(), 1);

    key(&mut s, KeyCode::Char('s'));
    assert_eq!(
        s.memory.filter.scope,
        Some(leveler_client_protocol::UiMemoryScope::Project)
    );
    assert_eq!(s.memory.visible_active().len(), 2);

    key(&mut s, KeyCode::Char('t'));
    assert_eq!(s.memory.filter.kind, crate::memory::KindFilter::UserFact);
    assert_eq!(s.memory.visible_active().len(), 1);
    key(&mut s, KeyCode::Char('t'));
    assert_eq!(s.memory.filter.kind, crate::memory::KindFilter::Experience);
    assert_eq!(s.memory.visible_active().len(), 1);
    assert_eq!(s.memory.visible_active()[0].id, "mem-exp");
    assert_eq!(s.memory.active.len(), 3);
}

#[test]
fn typing_a_query_does_not_trigger_page_actions() {
    let mut s = state();
    open_active(&mut s);
    key(&mut s, KeyCode::Char('/'));
    let effects = key(&mut s, KeyCode::Char('a'));
    assert!(
        effects.is_empty(),
        "a letter typed into the query line must not become a page action: {effects:?}"
    );
    assert_eq!(s.memory.filter.query, "a");
}

#[test]
fn the_detail_page_offers_the_record_text_and_its_identity() {
    let mut s = state();
    open_active(&mut s);
    assert!(key(&mut s, KeyCode::Enter).is_empty());
    assert_eq!(s.memory.view, MemoryView::Detail);

    let copy = key(&mut s, KeyCode::Char('y'));
    assert_eq!(
        copy,
        vec![Effect::CopyText {
            text: "构建使用 cargo 的正文".into(),
            what: crate::action::CopyWhat::Body,
        }],
        "the body must be copied exactly as the store returned it"
    );

    let reference = key(&mut s, KeyCode::Char('Y'));
    assert_eq!(
        reference,
        vec![Effect::CopyText {
            text: "project:mem-build".into(),
            what: crate::action::CopyWhat::Reference,
        }],
        "a copied id must name its scope; ids are only unique inside one store"
    );
}

// ----- detail actions -----------------------------------------------------

fn sent_commands(effects: &[Effect]) -> Vec<&ClientCommand> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Send(command) => Some(command),
            _ => None,
        })
        .collect()
}

#[test]
fn pinning_sends_one_command_and_refreshes_the_listing() {
    let mut s = state();
    open_active(&mut s);
    key(&mut s, KeyCode::Enter);
    assert_eq!(s.memory.view, MemoryView::Detail);

    let effects = key(&mut s, KeyCode::Char('p'));
    let commands = sent_commands(&effects);
    assert!(
        matches!(
            commands.first(),
            Some(ClientCommand::SetMemoryPin { pinned: true, id, .. }) if id == "mem-build"
        ),
        "{commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|command| matches!(command, ClientCommand::ListMemory { .. })),
        "the page must re-read the store rather than assume the write landed: {commands:?}"
    );
}

#[test]
fn forgetting_needs_two_presses_and_never_guesses_a_revision() {
    let mut s = state();
    open_active(&mut s);
    key(&mut s, KeyCode::Enter);

    let armed = key(&mut s, KeyCode::Char('f'));
    assert!(
        armed.is_empty(),
        "one keystroke must not archive a memory: {armed:?}"
    );
    assert!(s.memory.confirm.is_some());

    let confirmed = key(&mut s, KeyCode::Char('f'));
    let commands = sent_commands(&confirmed);
    assert!(
        matches!(
            commands.first(),
            Some(ClientCommand::ForgetMemory { id, .. }) if id == "mem-build"
        ),
        "{commands:?}"
    );
    assert!(s.memory.confirm.is_none());

    // Deprecate carries the revision the user was looking at.
    let mut s = state();
    open_active(&mut s);
    key(&mut s, KeyCode::Enter);
    key(&mut s, KeyCode::Char('d'));
    let confirmed = key(&mut s, KeyCode::Char('d'));
    let commands = sent_commands(&confirmed);
    assert!(
        matches!(
            commands.first(),
            Some(ClientCommand::DeprecateMemory { id, revision: 2, .. }) if id == "mem-build"
        ),
        "{commands:?}"
    );
}

#[test]
fn a_record_without_a_revision_cannot_be_deprecated() {
    let mut s = state();
    let effects = press(&mut s, "/memory");
    let query = list_query(&effects).expect("listing query");
    key(&mut s, KeyCode::Enter);
    let mut entry = entry_in(
        leveler_client_protocol::UiMemoryScope::Project,
        "mem-old-runtime",
        "旧运行时返回的记忆",
        leveler_client_protocol::UiMemoryKind::Note,
    );
    entry.revision = None;
    reduce(
        &mut s,
        Action::Runtime(list(Some(query), vec![entry], vec![], vec![])),
    );
    key(&mut s, KeyCode::Enter);
    key(&mut s, KeyCode::Char('d'));
    let confirmed = key(&mut s, KeyCode::Char('d'));
    assert!(
        confirmed.is_empty(),
        "a missing revision is a refusal, not a guessed overwrite: {confirmed:?}"
    );
    assert_eq!(s.memory.action_error, Some("revision"));
}

#[test]
fn any_other_key_disarms_a_pending_confirmation() {
    let mut s = state();
    open_active(&mut s);
    key(&mut s, KeyCode::Enter);
    key(&mut s, KeyCode::Char('f'));
    key(&mut s, KeyCode::Char('z'));
    assert!(s.memory.confirm.is_none());

    // Going back to the list also drops an armed action: nothing may stay
    // armed while the user is looking at something else.
    key(&mut s, KeyCode::Char('f'));
    assert!(s.memory.confirm.is_some());
    key(&mut s, KeyCode::Esc);
    assert!(s.memory.confirm.is_none());
    assert_eq!(s.memory.view, MemoryView::Active);
}

// ----- recall references --------------------------------------------------

#[test]
fn a_recall_names_the_records_the_turn_used_and_drops_forgotten_ones() {
    let mut s = state();
    open_active(&mut s);
    key(&mut s, KeyCode::Esc); // back to home
    assert_eq!(s.memory.view, MemoryView::Home);

    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::MemoryRecalled {
            count: 2,
            ids: vec!["mem-build".into(), "mem-gone".into()],
            experience_count: 0,
            injected: vec![
                leveler_client_protocol::UiMemoryReference {
                    id: "mem-build".into(),
                    scope: leveler_client_protocol::UiMemoryScope::Project,
                    title: Some("构建使用 cargo".into()),
                    mode: Some("standing".into()),
                },
                leveler_client_protocol::UiMemoryReference {
                    id: "mem-gone".into(),
                    scope: leveler_client_protocol::UiMemoryScope::Project,
                    title: Some("已被遗忘的旧约定".into()),
                    mode: Some("candidate".into()),
                },
            ],
        }),
    );

    let (rows, gone) = s.memory.recalled_rows();
    assert_eq!(rows.len(), 1, "only records still in the listing are shown");
    assert_eq!(rows[0].1.id, "mem-build");
    assert_eq!(
        gone, 1,
        "a forgotten record is counted, never re-shown by title"
    );
    assert_eq!(s.last_memory_recall, Some(2));

    // The transcript note keeps its count-only shape: ids and titles are not
    // copied into a durable line.
    let text = format!("{:?}", s.transcript.items());
    assert!(!text.contains("已被遗忘的旧约定"), "{text}");
}

// ----- the editor ---------------------------------------------------------

fn open_editor(s: &mut AppState) {
    open_active(s);
    key(s, KeyCode::Enter);
    assert_eq!(s.memory.view, MemoryView::Detail);
    key(s, KeyCode::Char('e'));
    assert_eq!(
        s.memory.view,
        MemoryView::Edit,
        "the detail page must offer the editor"
    );
}

fn ctrl(s: &mut AppState, character: char) -> Vec<Effect> {
    reduce(
        s,
        Action::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::CONTROL,
        )),
    )
}

fn typed(s: &mut AppState, text: &str) {
    for character in text.chars() {
        key(s, KeyCode::Char(character));
    }
}

#[test]
fn the_editor_opens_with_the_record_text_and_writes_only_on_save() {
    let mut s = state();
    open_editor(&mut s);
    let editor = s.memory.editor.as_ref().expect("an open editor");
    assert_eq!(editor.title_text(), "构建使用 cargo");
    assert_eq!(editor.body_text(), "构建使用 cargo 的正文");
    assert!(
        !editor.is_dirty(),
        "opening the editor is not an edit: nothing is dirty until typing"
    );

    // A no-op save must not write a pointless revision.
    assert!(ctrl(&mut s, 's').is_empty());
}

#[test]
fn a_title_edit_sends_the_opened_revision_and_the_whole_text() {
    let mut s = state();
    open_editor(&mut s);
    // The title buffer starts with the cursor at the end of the stored title.
    typed(&mut s, " 优先");

    let effects = ctrl(&mut s, 's');
    let commands = sent_commands(&effects);
    assert!(
        matches!(
            commands.first(),
            Some(ClientCommand::UpdateMemory { id, revision, title, body, .. })
                if id == "mem-build"
                    && *revision == 2
                    && title == "构建使用 cargo 优先"
                    && body == "构建使用 cargo 的正文"
        ),
        "the submit must carry the revision it opened and both fields: {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|command| matches!(command, ClientCommand::ListMemory { .. })),
        "the refresh follows the runtime's receipt, not the keypress: {commands:?}"
    );
}

#[test]
fn the_body_keeps_markdown_newlines_and_wide_text() {
    let mut s = state();
    open_editor(&mut s);
    key(&mut s, KeyCode::Tab);
    assert_eq!(
        s.memory.editor.as_ref().unwrap().field,
        crate::memory::EditField::Body
    );
    // Clear the body, then type a multi-line document with a code block.
    for _ in 0.."构建使用 cargo 的正文".chars().count() {
        key(&mut s, KeyCode::Backspace);
    }
    typed(&mut s, "第一行");
    key(&mut s, KeyCode::Enter);
    typed(&mut s, "```sh");
    key(&mut s, KeyCode::Enter);
    typed(&mut s, "cargo test");
    key(&mut s, KeyCode::Enter);
    typed(&mut s, "```");

    let effects = ctrl(&mut s, 's');
    let commands = sent_commands(&effects);
    let expected = "第一行\n```sh\ncargo test\n```";
    assert!(
        matches!(
            commands.first(),
            Some(ClientCommand::UpdateMemory { body, .. }) if body == expected
        ),
        "a Markdown body must be sent verbatim: {commands:?}"
    );
}

#[test]
fn a_burst_paste_lands_in_the_focused_buffer_not_the_composer() {
    let mut s = state();
    open_editor(&mut s);
    let paste = "粘贴的第一段\n粘贴的第二段";
    reduce(&mut s, Action::Paste(paste.into()));
    assert_eq!(
        s.memory.editor.as_ref().unwrap().title_text(),
        format!("构建使用 cargo{paste}")
    );
    assert!(
        s.composer.is_empty(),
        "the conversation composer must not receive an edit meant for memory"
    );

    // Coalesced typing (no bracketed paste) takes the same path.
    reduce(&mut s, Action::TextInput(" 打字".into()));
    assert!(
        s.memory
            .editor
            .as_ref()
            .unwrap()
            .title_text()
            .ends_with(" 打字")
    );
    assert!(s.composer.is_empty());
}

#[test]
fn esc_keeps_unsent_text_until_the_second_press() {
    let mut s = state();
    open_editor(&mut s);
    typed(&mut s, "改");

    assert!(key(&mut s, KeyCode::Esc).is_empty());
    assert_eq!(s.memory.view, MemoryView::Edit, "one Esc must not discard");
    assert!(s.memory.editor.as_ref().unwrap().confirm_discard);

    key(&mut s, KeyCode::Esc);
    assert_eq!(s.memory.view, MemoryView::Detail);
    assert!(s.memory.editor.is_none(), "the second Esc discards");
}

#[test]
fn an_unchanged_editor_leaves_on_one_esc() {
    let mut s = state();
    open_editor(&mut s);
    key(&mut s, KeyCode::Esc);
    assert_eq!(s.memory.view, MemoryView::Detail);
}

#[test]
fn a_blank_title_is_refused_before_it_reaches_the_runtime() {
    let mut s = state();
    open_editor(&mut s);
    for _ in 0.."构建使用 cargo".chars().count() {
        key(&mut s, KeyCode::Backspace);
    }
    let effects = ctrl(&mut s, 's');
    assert!(effects.is_empty(), "a nameless memory must not be written");
    assert_eq!(
        s.memory.editor.as_ref().unwrap().status,
        crate::memory::EditStatus::Failed
    );
}

#[test]
fn a_receipt_settles_the_editor_and_only_then_is_the_listing_asked() {
    let mut s = state();
    open_editor(&mut s);
    typed(&mut s, " 优先");
    ctrl(&mut s, 's');
    assert_eq!(
        s.memory.editor.as_ref().unwrap().status,
        crate::memory::EditStatus::Submitting
    );

    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "已更新记忆 [mem-build]: update".into(),
        }),
    );
    assert!(
        s.memory.editor.is_none(),
        "a successful write closes the editor"
    );
    assert_eq!(s.memory.view, MemoryView::Detail);
    assert!(
        list_query(&effects).is_some(),
        "the page must repaint from the store: {effects:?}"
    );
}

#[test]
fn a_refused_save_keeps_the_user_text_and_names_the_new_revision() {
    let mut s = state();
    open_editor(&mut s);
    typed(&mut s, " 优先");
    ctrl(&mut s, 's');

    // Another process wrote revision 3 in the meantime: the runtime refuses.
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Warning,
            message: "update conflict [mem-build]: memory conflict: current revision 3, expected 2"
                .into(),
        }),
    );
    assert_eq!(
        s.memory.editor.as_ref().unwrap().status,
        crate::memory::EditStatus::Failed
    );
    assert_eq!(
        s.memory.editor.as_ref().unwrap().title_text(),
        "构建使用 cargo 优先",
        "a refused save must not lose what was typed"
    );

    // The proving listing says the record really moved.
    let query = list_query(&effects).expect("a refusal still refreshes");
    let mut moved = entry_in(
        leveler_client_protocol::UiMemoryScope::Project,
        "mem-build",
        "构建使用 cargo",
        leveler_client_protocol::UiMemoryKind::Preference,
    );
    moved.revision = Some(3);
    reduce(
        &mut s,
        Action::Runtime(list(Some(query), vec![moved], vec![], vec![])),
    );

    assert_eq!(
        s.memory.editor.as_ref().unwrap().status,
        crate::memory::EditStatus::Conflict { current: Some(3) },
        "a conflict is concluded from the store's revision, never assumed"
    );
    assert_eq!(
        s.memory.editor.as_ref().unwrap().title_text(),
        "构建使用 cargo 优先"
    );
}

#[test]
fn reloading_the_current_version_needs_two_presses() {
    let mut s = state();
    open_editor(&mut s);
    typed(&mut s, " 优先");
    ctrl(&mut s, 's');
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Warning,
            message: "update conflict [mem-build]".into(),
        }),
    );
    let query = list_query(&effects).expect("a refusal still refreshes");
    let mut moved = entry_in(
        leveler_client_protocol::UiMemoryScope::Project,
        "mem-build",
        "构建改用 cargo",
        leveler_client_protocol::UiMemoryKind::Preference,
    );
    moved.revision = Some(3);
    reduce(
        &mut s,
        Action::Runtime(list(Some(query), vec![moved], vec![], vec![])),
    );
    assert!(matches!(
        s.memory.editor.as_ref().unwrap().status,
        crate::memory::EditStatus::Conflict { .. }
    ));

    ctrl(&mut s, 'r');
    assert!(
        s.memory.editor.as_ref().unwrap().confirm_reload,
        "the first press only asks"
    );
    ctrl(&mut s, 'r');
    let editor = s.memory.editor.as_ref().unwrap();
    assert_eq!(editor.title_text(), "构建改用 cargo");
    assert_eq!(editor.revision, 3);
    assert_eq!(editor.status, crate::memory::EditStatus::Editing);
}

#[test]
fn a_saved_edit_is_what_the_next_open_shows() {
    let mut s = state();
    open_editor(&mut s);
    typed(&mut s, " 优先");
    ctrl(&mut s, 's');
    let effects = reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::Notification {
            level: leveler_client_protocol::NotificationLevel::Info,
            message: "已更新记忆 [mem-build]".into(),
        }),
    );
    let query = list_query(&effects).expect("a success refreshes");
    // The store's answer, with the new revision and the new title.
    let mut updated = entry_in(
        leveler_client_protocol::UiMemoryScope::Project,
        "mem-build",
        "构建使用 cargo 优先",
        leveler_client_protocol::UiMemoryKind::Preference,
    );
    updated.revision = Some(3);
    reduce(
        &mut s,
        Action::Runtime(list(Some(query), vec![updated], vec![], vec![])),
    );

    key(&mut s, KeyCode::Esc);
    key(&mut s, KeyCode::Enter);
    key(&mut s, KeyCode::Char('e'));
    let editor = s.memory.editor.as_ref().unwrap();
    assert_eq!(editor.title_text(), "构建使用 cargo 优先");
    assert_eq!(
        editor.revision, 3,
        "a reopened record edits the revision the store returned"
    );
}

#[test]
fn editing_is_not_a_list_shortcut() {
    let mut s = state();
    open_editor(&mut s);
    // `d`, `f`, `p` are letters in a buffer, not lifecycle actions.
    typed(&mut s, "dfp");
    assert!(s.memory.confirm.is_none());
    assert!(s.memory.editor.is_some());
    assert_eq!(
        s.memory.editor.as_ref().unwrap().title_text(),
        "构建使用 cargodfp"
    );
}
