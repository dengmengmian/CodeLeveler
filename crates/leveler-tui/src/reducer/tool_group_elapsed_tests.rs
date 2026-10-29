use super::reduce;
use crate::{
    action::Action,
    state::{AppState, Boot},
};
use leveler_client_protocol::{RuntimeEvent, SessionId};

fn state() -> AppState {
    let mut s = AppState::new(
        crate::theme::Theme::no_color(),
        Boot {
            session_id: SessionId::new("elapsed"),
            user: "u".into(),
            version: "test".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 8192,
            locale: crate::i18n::Locale::Zh,
            untrusted_config: vec![],
            model_notice: None,
            thinking: None,
        },
    );
    s.status = leveler_client_protocol::RuntimeStatus::Busy;
    s.size = (120, 30);
    s.conv.rect = Some((0, 0, 120, 20));
    s.transcript.push_user("run tools".into());
    s
}

fn event(s: &mut AppState, value: serde_json::Value) {
    reduce(s, Action::Runtime(serde_json::from_value(value).unwrap()));
}
fn started(s: &mut AppState, id: &str, start: u64, step: u32) {
    event(
        s,
        serde_json::json!({"type":"tool_call_started", "id":id,
        "name":"run_command", "arguments":"{\"cmd\":\"echo test\"}", "parallel":true,
        "model_step":step}),
    );
    event(
        s,
        serde_json::json!({"type":"tool_timing_updated", "id":id, "timing":{"clock_id":"turn","started_ms":start,"elapsed_ms":0,"finished":false}}),
    );
}
fn completed(s: &mut AppState, id: &str, start: u64, duration: u64, ok: bool) {
    event(
        s,
        serde_json::json!({"type":"tool_timing_updated", "id":id,"timing":{"clock_id":"turn","started_ms":start,"elapsed_ms":duration,"finished":true}}),
    );
    event(
        s,
        serde_json::json!({"type":"tool_call_completed", "id":id, "ok":ok,
        "preview":"output", "duration_ms":duration}),
    );
}
fn receipts(s: &AppState, width: usize) -> Vec<String> {
    s.conversation_lines(width)
        .iter()
        .map(crate::selection::line_to_plain)
        .filter(|line| line.contains('▸') || line.contains('▾'))
        .collect()
}

#[test]
fn elapsed_live_then_success_freezes_without_turn_time() {
    let mut s = state();
    s.elapsed_secs = 200;
    started(&mut s, "a", 50_000, 1);
    s.elapsed_secs = 305;
    assert!(receipts(&s, 120)[0].ends_with(" · 1m 45s"));
    completed(&mut s, "a", 50_000, 106_000, true);
    s.elapsed_secs = 900;
    assert!(receipts(&s, 120)[0].ends_with(" · 1m 46s"));
}

#[test]
fn elapsed_parallel_partial_failure_keeps_running_and_uses_the_span() {
    let mut s = state();
    started(&mut s, "a", 10_000, 1);
    s.elapsed_secs = 5;
    started(&mut s, "b", 15_000, 1);
    s.elapsed_secs = 8;
    completed(&mut s, "a", 10_000, 8_000, false);
    s.elapsed_secs = 12;
    let head = &receipts(&s, 120)[0];
    assert!(
        head.contains("正在执行 1 项") && head.contains("1 个失败"),
        "{head}"
    );
    assert!(head.ends_with(" · 12s"), "{head}");
    completed(&mut s, "b", 15_000, 10_000, true);
    s.elapsed_secs = 100;
    assert!(receipts(&s, 120)[0].ends_with(" · 15s"));
}

#[test]
fn elapsed_serial_includes_gaps_and_separate_receipts_do_not_share_time() {
    let mut s = state();
    started(&mut s, "a", 1_000, 1);
    completed(&mut s, "a", 1_000, 2_000, true);
    s.elapsed_secs = 7;
    started(&mut s, "b", 8_000, 1);
    completed(&mut s, "b", 8_000, 3_000, false);
    assert!(receipts(&s, 120)[0].ends_with(" · 10s"));
    s.transcript.push_user("next group".into());
    started(&mut s, "c", 50_000, 2);
    completed(&mut s, "c", 50_000, 4_000, true);
    let heads = receipts(&s, 120);
    assert!(
        heads[0].ends_with(" · 10s") && heads[1].ends_with(" · 4s"),
        "{heads:?}"
    );
}

#[test]
fn elapsed_legacy_events_do_not_invent_a_clock() {
    let mut s = state();
    event(
        &mut s,
        serde_json::json!({"type":"tool_call_started","id":"old","name":"run_command","arguments":"{}"}),
    );
    event(
        &mut s,
        serde_json::json!({"type":"tool_call_completed","id":"old","ok":true,"preview":"ok","duration_ms":0}),
    );
    assert!(!receipts(&s, 120)[0].ends_with(" · 0s"));
}

#[test]
fn elapsed_waiting_for_approval_is_not_execution_and_failure_freezes() {
    let mut s = state();
    event(
        &mut s,
        serde_json::json!({"type":"tool_call_started","id":"a","name":"run_command","arguments":"{}"}),
    );
    s.elapsed_secs = 120;
    assert!(!receipts(&s, 120)[0].contains("120s"));
    event(
        &mut s,
        serde_json::json!({"type":"tool_timing_updated","id":"a",
        "timing":{"clock_id":"turn","started_ms":50_000,"elapsed_ms":0,"finished":false}}),
    );
    s.elapsed_secs = 138;
    assert!(receipts(&s, 120)[0].ends_with(" · 18s"));
    completed(&mut s, "a", 50_000, 18_000, false);
    s.elapsed_secs = 999;
    let head = &receipts(&s, 120)[0];
    assert!(
        head.contains("1 个失败") && head.ends_with(" · 18s"),
        "{head}"
    );
    let line = s
        .conversation_lines(120)
        .iter()
        .find(|line| crate::selection::line_to_plain(line).contains("1 个失败"))
        .unwrap()
        .clone();
    assert!(line.spans.iter().any(
        |span| span.content.contains("1 个失败") && span.style.fg == Some(s.theme.status.error)
    ));
}

#[test]
fn elapsed_resume_uses_snapshot_elapsed_and_never_restarts_finished_work() {
    let mut s = state();
    event(
        &mut s,
        serde_json::json!({"type":"session_opened","session":{
        "id":"elapsed","repository":"/repo","goal":"resume","model":null,"mode":"assisted",
        "branch":null,"status":"running","messages":[],"active_tools":[{
            "id":"a","name":"run_command","arguments":"{}","elapsed_ms":200_000,
            "timing":{"clock_id":"turn","started_ms":40_000,"elapsed_ms":75_000,"finished":false}}]}}),
    );
    assert!(receipts(&s, 120)[0].ends_with(" · 1m 15s"));
    s.elapsed_secs += 5;
    assert!(receipts(&s, 120)[0].ends_with(" · 1m 20s"));
    completed(&mut s, "a", 40_000, 80_000, true);
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    s.elapsed_secs = 500;
    assert!(receipts(&s, 120)[0].ends_with(" · 1m 20s"));
}

#[test]
fn elapsed_history_reloads_deterministically_and_missing_boundaries_stay_unknown() {
    for known in [true, false] {
        let mut s = state();
        s.status = leveler_client_protocol::RuntimeStatus::Idle;
        s.history_query = Some(leveler_client_protocol::CommandId::new("history"));
        let mut events = vec![
            serde_json::json!({"type":"tool_call_started","id":"a","name":"run_command","arguments":"{}"}),
        ];
        if known {
            events.push(serde_json::json!({"type":"tool_timing_updated","id":"a","timing":{"clock_id":"turn","started_ms":5_000,"elapsed_ms":0,"finished":false}}));
            events.push(serde_json::json!({"type":"tool_timing_updated","id":"a","timing":{"clock_id":"turn","started_ms":5_000,"elapsed_ms":72_000,"finished":true}}));
        }
        events.push(serde_json::json!({"type":"tool_call_completed","id":"a","ok":false,"preview":"failed","duration_ms":0}));
        events.push(serde_json::json!({"type":"turn_completed"}));
        // Wall-clock chronology can be absent or reversed; monotonic execution
        // facts remain authoritative and a replay never samples the live clock.
        let entries: Vec<_> = events.into_iter().enumerate().map(|(i,event)|
            serde_json::json!({"turn_elapsed_ms":1000_u64.saturating_sub(i as u64*100),"turn_start":i==0,"event":event})).collect();
        event(
            &mut s,
            serde_json::json!({"type":"session_history_loaded","query_id":"history","session_id":"elapsed","entries":entries,"omitted_turns":0}),
        );
        let before = receipts(&s, 120);
        s.elapsed_secs = 999;
        assert_eq!(receipts(&s, 120), before);
        if known {
            assert!(before[0].ends_with(" · 1m 12s"), "{before:?}");
        } else {
            assert!(!before[0].ends_with(" · 0s"));
        }
    }
}

#[test]
fn elapsed_ticks_reuse_same_second_and_keep_scroll_fold_geometry_and_details() {
    let mut s = state();
    for i in 0..30 {
        s.transcript.push_note(format!("older history {i}"));
    }
    started(&mut s, "a", 0, 1);
    s.elapsed_secs = 8;
    s.conv.auto_scroll = false;
    s.conv.scroll = 3;
    let before = s.conversation_lines(120);
    s.tick += 1;
    let same = s.conversation_lines(120);
    assert!(
        std::rc::Rc::ptr_eq(&before, &same),
        "subsecond spinner frames must reuse the cached projection"
    );
    s.elapsed_secs = 9;
    crate::conversation::sync_scroll(&mut s);
    let next = s.conversation_lines(120);
    assert_eq!(next.len(), before.len());
    assert_eq!(s.conv.scroll, 3);
    let group = s.transcript.last_tool_group_index().unwrap();
    assert_eq!(
        crate::conversation::interaction::toggle_fold(&mut s, group),
        Some(true)
    );
    // Freeze completion so member clocks/output remain the exact same content.
    completed(&mut s, "a", 0, 10_000, true);
    let expanded = s.conversation_lines(120);
    s.elapsed_secs = 999;
    let frozen = s.conversation_lines(120);
    assert_eq!(*expanded, *frozen);
    assert_eq!(
        crate::conversation::interaction::toggle_fold(&mut s, group),
        Some(false)
    );
    assert!(receipts(&s, 120)[0].starts_with('▸'));
    for width in [18, 32, 56] {
        let lines = s.conversation_lines(width);
        assert!(lines.iter().all(|line| line.width() <= width));
        assert!(receipts(&s, width)[0].contains('▸'));
    }
}

#[test]
fn elapsed_missing_execution_end_and_overflow_are_not_fabricated() {
    let mut s = state();
    started(&mut s, "a", 0, 1);
    s.elapsed_secs = 8;
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCancelled));
    assert!(!receipts(&s, 120)[0].ends_with(" · 8s"));
    s.transcript.push_user("next".into());
    started(&mut s, "b", u64::MAX - 1, 2);
    completed(&mut s, "b", u64::MAX - 1, 1000, true);
    assert!(!receipts(&s, 120)[1].ends_with(" · 1s"));
}

#[test]
fn elapsed_does_not_combine_unrelated_runtime_clocks() {
    let mut s = state();
    started(&mut s, "a", 5000, 1);
    completed(&mut s, "a", 5000, 1000, true);
    started(&mut s, "b", 0, 2);
    event(
        &mut s,
        serde_json::json!({"type":"tool_timing_updated","id":"b","timing":{
        "clock_id":"different-runtime","started_ms":0,"elapsed_ms":10_000,"finished":true}}),
    );
    event(
        &mut s,
        serde_json::json!({"type":"tool_call_completed","id":"b","ok":true,"preview":"ok","duration_ms":10_000}),
    );
    assert!(!receipts(&s, 120)[0].ends_with(" · 10s"));
}

#[test]
fn elapsed_keeps_counting_existing_execution_when_another_call_is_queued() {
    let mut s = state();
    started(&mut s, "a", 0, 1);
    event(
        &mut s,
        serde_json::json!({"type":"tool_call_started","id":"queued","name":"run_command","arguments":"{}","model_step":1}),
    );
    s.elapsed_secs = 2;
    completed(&mut s, "a", 0, 2000, false);
    s.elapsed_secs = 12;
    assert!(receipts(&s, 120)[0].ends_with(" · 12s"));
}

#[test]
fn elapsed_detached_receipts_do_not_mislabel_spawn_latency_as_process_runtime() {
    let mut s = state();
    event(
        &mut s,
        serde_json::json!({"type":"tool_call_started","id":"bg","name":"run_command","arguments":"{\"program\":\"server\",\"background\":true}"}),
    );
    event(
        &mut s,
        serde_json::json!({"type":"tool_timing_updated","id":"bg","timing":{"clock_id":"turn","started_ms":0,"elapsed_ms":25,"finished":true}}),
    );
    event(
        &mut s,
        serde_json::json!({"type":"tool_call_completed","id":"bg","ok":true,"duration_ms":25,"preview":"task_id: bg-1\nstatus: running"}),
    );
    s.elapsed_secs = 100;
    let head = &receipts(&s, 120)[0];
    assert!(head.contains("正在执行"));
    assert!(!head.ends_with(" · 0s"), "{head}");
}

#[test]
fn elapsed_resume_preserves_finished_members_of_the_same_execution_group() {
    let mut s = state();
    let history = vec![
        serde_json::json!({"type":"tool_call_started","id":"a","name":"run_command","arguments":"{}","model_step":1}),
        serde_json::json!({"type":"tool_timing_updated","id":"a","timing":{"clock_id":"turn","started_ms":0,"elapsed_ms":0,"finished":false}}),
        serde_json::json!({"type":"tool_call_started","id":"b","name":"run_command","arguments":"{}","model_step":1}),
        serde_json::json!({"type":"tool_timing_updated","id":"b","timing":{"clock_id":"turn","started_ms":5000,"elapsed_ms":0,"finished":false}}),
        serde_json::json!({"type":"tool_timing_updated","id":"a","timing":{"clock_id":"turn","started_ms":0,"elapsed_ms":10_000,"finished":true}}),
        serde_json::json!({"type":"tool_call_completed","id":"a","ok":false,"preview":"failed","duration_ms":10_000}),
    ];
    let entries: Vec<_> = history.into_iter().enumerate().map(|(i,event)|serde_json::json!({"event":event,"turn_elapsed_ms":i*1000,"turn_start":i==0})).collect();
    event(
        &mut s,
        serde_json::json!({"type":"session_opened","session":{
        "id":"elapsed","repository":"/repo","goal":"resume","model":null,"mode":"assisted",
        "branch":null,"status":"running","messages":[],
        "history":{"entries":entries,"omitted_turns":0},
        "active_tools":[{"id":"b","name":"run_command","arguments":"{}","model_step":1,"elapsed_ms":35000,
            "timing":{"clock_id":"turn","started_ms":5000,"elapsed_ms":35000,"finished":false}}]}}),
    );
    let head = &receipts(&s, 120)[0];
    assert!(
        head.contains("2 个命令") && head.contains("1 个失败") && head.ends_with(" · 40s"),
        "{head}"
    );
    s.elapsed_secs += 5;
    assert!(receipts(&s, 120)[0].ends_with(" · 45s"));
    completed(&mut s, "b", 5000, 40_000, true);
    s.elapsed_secs = 999;
    assert!(receipts(&s, 120)[0].ends_with(" · 45s"));
}

#[test]
fn elapsed_confirmed_and_unknown_stops_freeze_using_actual_execution_end() {
    for stop in ["confirmed", "unconfirmed"] {
        let mut s = state();
        started(&mut s, "a", 1000, 1);
        event(
            &mut s,
            serde_json::json!({"type":"tool_timing_updated","id":"a","timing":{"clock_id":"turn","started_ms":1000,"elapsed_ms":18_000,"finished":true}}),
        );
        event(
            &mut s,
            serde_json::json!({"type":"tool_call_completed","id":"a","ok":false,"preview":"stopped","duration_ms":18_000,"stop":stop}),
        );
        let before = receipts(&s, 120);
        s.elapsed_secs = 999;
        assert_eq!(receipts(&s, 120), before);
        assert!(before[0].ends_with(" · 18s"), "{before:?}");
        assert!(!before[0].contains("1 个失败"));
    }
}

#[test]
fn elapsed_reuses_the_existing_product_format_and_preserves_two_failures() {
    for (ms, label) in [
        (8000, "8s"),
        (45000, "45s"),
        (65000, "1m 05s"),
        (750000, "12m 30s"),
        (3735000, "62m 15s"),
    ] {
        let mut s = state();
        started(&mut s, "a", 1000, 1);
        started(&mut s, "b", 1000, 1);
        completed(&mut s, "a", 1000, ms, false);
        completed(&mut s, "b", 1000, ms, false);
        let head = &receipts(&s, 120)[0];
        assert!(
            head.contains("2 个失败") && head.ends_with(&format!(" · {label}")),
            "{head}"
        );
    }
}

#[test]
fn elapsed_resume_waiting_for_next_dispatch_omits_unknown_current_coordinate() {
    let mut s = state();
    let history = [
        serde_json::json!({"type":"tool_call_started","id":"a","name":"run_command","arguments":"{}","parallel":false,"model_step":1}),
        serde_json::json!({"type":"tool_timing_updated","id":"a","timing":{"clock_id":"turn","started_ms":0,"elapsed_ms":10000,"finished":true}}),
        serde_json::json!({"type":"tool_call_completed","id":"a","ok":false,"preview":"failed","duration_ms":10000}),
    ];
    let entries: Vec<_> = history.into_iter().enumerate().map(|(i,event)|serde_json::json!({"event":event,"turn_elapsed_ms":i*1000,"turn_start":i==0})).collect();
    event(
        &mut s,
        serde_json::json!({"type":"session_opened","session":{
        "id":"elapsed","repository":"/repo","goal":"resume","model":null,"mode":"assisted",
        "branch":null,"status":"running","messages":[],
        "history":{"entries":entries,"omitted_turns":0},
        "active_tools":[{"id":"b","name":"run_command","arguments":"{}","model_step":1,"elapsed_ms":35000}]}}),
    );
    assert!(
        receipts(&s, 120)[0].ends_with("1 个失败"),
        "an old completion cannot locate the resumed pending group's current execution coordinate: {:?}",
        receipts(&s, 120)
    );
    event(
        &mut s,
        serde_json::json!({"type":"tool_timing_updated","id":"b","timing":{"clock_id":"turn","started_ms":40000,"elapsed_ms":0,"finished":false}}),
    );
    assert!(receipts(&s, 120)[0].ends_with(" · 40s"));
    s.elapsed_secs += 5;
    assert!(receipts(&s, 120)[0].ends_with(" · 45s"));
}
