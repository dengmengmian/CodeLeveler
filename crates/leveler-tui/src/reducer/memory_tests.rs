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
