//! Beta Product Closure, Phase B: render the ten product scenarios headlessly
//! so their frames can be read instead of guessed at.
//!
//! Every scenario is driven through the real reducer and the real renderer.
//! The event shapes — how many reads, how long the plan is, which tool fails —
//! are taken from the runs recorded under
//! `evals/baselines/beta-phaseA-98d21bc/` and `progressive-comparative-a1534eb/`.
//!
//!   cargo test -p leveler-tui --test product_scenarios -- --ignored --nocapture

use leveler_client_protocol::{
    CheckState, MessageId, PlanStepStatus, RuntimeEvent, SessionId, ToolCallId, UiCheck, UiDiff,
    UiDiffFile, UiMessage, UiPlan, UiPlanStep, UiRole, UiSessionSnapshot, UiVerification,
};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const W: u16 = 100;
const H: u16 = 34;

fn dump(state: &mut AppState, label: &str) {
    let backend = TestBackend::new(W, H);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    println!("\n===== {label} =====");
    println!("+{}+", "-".repeat(W as usize));
    for y in 0..H {
        let mut line = String::new();
        let mut x = 0u16;
        while x < W {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            line.push_str(sym);
            x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
        }
        println!("|{}|", line.trim_end());
    }
    println!("+{}+", "-".repeat(W as usize));
}

fn opened(goal: &str) -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".into(),
            version: "0.2.0-beta.1".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 1_048_576,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            reasoning_effort: None,
        },
    );
    let snap = UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "~/Develop/navsvc".into(),
        goal: goal.into(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/deepseek-v4-flash"),
        mode: leveler_client_protocol::PermissionProfile::Assisted,
        branch: Some("main".into()),
        status: "idle".into(),
        messages: Vec::new(),
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: None,
        active_tools: Vec::new(),
        plan: None,
        verification: None,
        diff: None,
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        reasoning: None,
        work_profile: None,
        collaboration: None,
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: goal.into(),
                ordinal: None,
            },
        }),
    );
    s
}

fn say(s: &mut AppState, id: &str, text: &str) {
    let m = MessageId::new(id);
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: m.clone(),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: m.clone(),
            delta: text.into(),
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted { message_id: m }),
    );
}

fn tool(s: &mut AppState, id: &str, name: &str, args: &str, ok: bool, preview: &str, ms: u64) {
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: name.into(),
            arguments: args.into(),
            parallel: false,
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new(id),
            ok,
            preview: preview.into(),
            duration_ms: ms,
            applied_diff: None,
        }),
    );
}

/// `apply_patch` takes a `patch` envelope, not a path — the shape the runs
/// under evals/baselines/beta-phaseA-98d21bc actually sent.
fn edit(s: &mut AppState, id: &str, path: &str, applied: &str) {
    let body = applied
        .lines()
        .filter(|l| l.starts_with('+') || l.starts_with('-'))
        .filter(|l| !l.starts_with("+++") && !l.starts_with("---"))
        .collect::<Vec<_>>()
        .join("\n");
    let patch = format!("*** Begin Patch\n*** Update File: {path}\n@@\n{body}\n*** End Patch");
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new(id),
            name: "apply_patch".into(),
            arguments: serde_json::json!({ "patch": patch }).to_string(),
            parallel: false,
        }),
    );
    reduce(
        s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new(id),
            ok: true,
            preview: format!("patched {path}"),
            duration_ms: 9,
            applied_diff: Some(applied.into()),
        }),
    );
}

fn plan(steps: &[(&str, PlanStepStatus)]) -> UiPlan {
    UiPlan {
        steps: steps
            .iter()
            .enumerate()
            .map(|(i, (d, st))| UiPlanStep {
                index: i,
                description: (*d).into(),
                status: *st,
            })
            .collect(),
    }
}

// --- 1. simple edit -------------------------------------------------------

#[test]
#[ignore = "manual product harness"]
fn s01_simple_edit() {
    // rust-first-even: read, list, read, patch, cargo test, update_goal.
    let mut s = opened("给 first_even 加上空切片返回 None 的处理");
    say(&mut s, "a1", "先看一眼现在的实现。");
    tool(
        &mut s,
        "t1",
        "read_file",
        "{\"path\":\"src/lib.rs\"}",
        true,
        "pub fn first_even(v: &[i32]) -> i32 {",
        1,
    );
    tool(
        &mut s,
        "t2",
        "list_files",
        r#"{"max_depth":3,"path":"."}"#,
        true,
        "src/lib.rs\nCargo.toml",
        17,
    );
    edit(
        &mut s,
        "t3",
        "src/lib.rs",
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -3,6 +3,9 @@\n-pub fn first_even(v: &[i32]) -> i32 {\n+pub fn first_even(v: &[i32]) -> Option<i32> {\n+    if v.is_empty() {\n+        return None;\n+    }\n",
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::DiffUpdated {
            diff: UiDiff {
                files: vec![UiDiffFile {
                    path: "src/lib.rs".into(),
                    added: 9,
                    removed: 3,
                    patch: None,
                }],
            },
        }),
    );
    dump(&mut s, "01 simple edit / mid-flight");
    tool(
        &mut s,
        "t4",
        "run_command",
        r#"{"program":"cargo","args":["test"]}"#,
        true,
        "test result: ok. 4 passed",
        1438,
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::VerificationUpdated {
            verification: UiVerification {
                checks: vec![UiCheck {
                    name: "cargo test".into(),
                    status: CheckState::Passed,
                    evidence: None,
                }],
                passed: Some(true),
            },
        }),
    );
    say(&mut s, "a2", "空切片现在返回 None，测试通过。");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    dump(&mut s, "01 simple edit / terminal Completed");
}

// --- 2. medium multi-file + 4. many reads --------------------------------

#[test]
#[ignore = "manual product harness"]
fn s02_multifile_and_s04_exploration_heavy() {
    // n3-caller-propagation: 14 reads, 4 greps, 1 patch — the shape that
    // decides whether exploration drowns the narrative.
    let mut s = opened("把 refund 的新参数传播到所有调用点");
    say(&mut s, "a1", "先定位所有调用点。");
    for i in 0..4 {
        tool(
            &mut s,
            &format!("g{i}"),
            "grep",
            "{\"pattern\":\"refund(\"}",
            true,
            "internal/billing/refund.go:41\ninternal/api/handler.go:88",
            16,
        );
    }
    for i in 0..14 {
        tool(
            &mut s,
            &format!("r{i}"),
            "read_file",
            &format!("{{\"path\":\"internal/pkg{i}/mod.go\"}}"),
            true,
            "package pkg\n\nfunc Refund(ctx context.Context, id string) error {",
            1,
        );
    }
    dump(
        &mut s,
        "04 exploration-heavy / 18 read+search calls, no mutation yet",
    );
    edit(
        &mut s,
        "p1",
        "internal/billing/refund.go",
        "--- a/internal/billing/refund.go\n+++ b/internal/billing/refund.go\n@@ -41,7 +41,7 @@\n-func Refund(ctx context.Context, id string) error {\n+func Refund(ctx context.Context, id string, reason Reason) error {\n",
    );
    edit(
        &mut s,
        "p2",
        "internal/api/handler.go",
        "--- a/internal/api/handler.go\n+++ b/internal/api/handler.go\n@@ -88,7 +88,7 @@\n-    return billing.Refund(ctx, id)\n+    return billing.Refund(ctx, id, reason)\n",
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::DiffUpdated {
            diff: UiDiff {
                files: vec![
                    UiDiffFile {
                        path: "internal/billing/refund.go".into(),
                        added: 1,
                        removed: 1,
                        patch: None,
                    },
                    UiDiffFile {
                        path: "internal/api/handler.go".into(),
                        added: 1,
                        removed: 1,
                        patch: None,
                    },
                ],
            },
        }),
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    dump(&mut s, "02 multi-file / after two edits and terminal");
}

// --- 5. with plan, long plan viewport -------------------------------------

#[test]
#[ignore = "manual product harness"]
fn s05_long_plan() {
    use PlanStepStatus::*;
    let mut s = opened("给 yq 加 --doc-count，含文档与测试");
    let steps: Vec<(&str, PlanStepStatus)> = vec![
        ("读 cmd/ 下的 flag 注册", Done),
        ("找到 evaluate 的输出路径", Done),
        ("加 --doc-count flag", Done),
        ("在 printer 里累计文档数", Done),
        ("处理 --null-input 的特例", Done),
        ("补 pkg/yqlib 的单测", Running),
        ("更新 README 的 flag 表", Pending),
        ("跑 go test ./...", Pending),
        ("跑 golangci-lint", Pending),
    ];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated { plan: plan(&steps) }),
    );
    say(&mut s, "a1", "计划已更新，现在补单测。");
    tool(
        &mut s,
        "t1",
        "read_file",
        "{\"path\":\"pkg/yqlib/printer_test.go\"}",
        true,
        "func TestPrinter",
        1,
    );
    dump(&mut s, "05 long plan / 9 steps, step 6 running");
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    dump(
        &mut s,
        "05 long plan / terminal Completed with plan at 5 done, 1 running, 3 pending",
    );
}

// --- 6. no plan -----------------------------------------------------------

#[test]
#[ignore = "manual product harness"]
fn s06_no_plan() {
    let mut s = opened("把 summary.go 里的 zero-value 行去掉");
    say(&mut s, "a1", "这个改动很小，直接做。");
    edit(
        &mut s,
        "p1",
        "internal/report/summary.go",
        "--- a/internal/report/summary.go\n+++ b/internal/report/summary.go\n@@ -22,6 +22,9 @@\n+\tif record.Value == 0 {\n+\t\treturn\n+\t}\n",
    );
    reduce(&mut s, Action::Runtime(RuntimeEvent::TurnCompleted));
    dump(&mut s, "06 no plan / terminal");
}

// --- 7. blocked -----------------------------------------------------------

#[test]
#[ignore = "manual product harness"]
fn s07_blocked() {
    let mut s = opened("让所有 zero-value 记录都不出现在 summary 里");
    say(&mut s, "a1", "先确认现有测试。");
    tool(
        &mut s,
        "t1",
        "run_command",
        r#"{"program":"go","args":["test","./..."]}"#,
        true,
        "ok navsvc/internal/report",
        214,
    );
    say(
        &mut s,
        "a2",
        "这个要求和 TestZeroValueRecordsAreCounted 直接冲突：那个测试断言 idle count=1 total=0 必须出现在渲染结果里。任务要求它消失。两者不能同时成立，我没有改动代码。",
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnIncomplete {
            reason: "任务要求与 internal/report/zero_test.go 的既有断言冲突".into(),
        }),
    );
    dump(&mut s, "07 blocked / TurnIncomplete, tree untouched");
}

// --- 8. failed command + 9. verification failure -------------------------

#[test]
#[ignore = "manual product harness"]
fn s08_failed_command_and_s09_verification_failure() {
    let mut s = opened("修 refund 的并发 bug");
    edit(
        &mut s,
        "p1",
        "internal/billing/refund.go",
        "--- a/internal/billing/refund.go\n+++ b/internal/billing/refund.go\n@@ -55,6 +55,7 @@\n+\tmu.Lock()\n",
    );
    tool(
        &mut s,
        "t1",
        "run_command",
        r#"{"program":"go","args":["build","./..."]}"#,
        false,
        "internal/billing/refund.go:56:2: undefined: mu",
        820,
    );
    dump(&mut s, "08 failed command / build error");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::VerificationUpdated {
            verification: UiVerification {
                checks: vec![
                    UiCheck {
                        name: "go build ./...".into(),
                        status: CheckState::Failed,
                        evidence: Some("internal/billing/refund.go:56:2: undefined: mu".into()),
                    },
                    UiCheck {
                        name: "go test ./...".into(),
                        status: CheckState::Skipped,
                        evidence: None,
                    },
                ],
                passed: Some(false),
            },
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TurnCompletedChecksFailed {
            reason: "go build ./... 失败".into(),
        }),
    );
    dump(
        &mut s,
        "09 verification failure / TurnCompletedChecksFailed",
    );
}

// --- 3. long task + model wait + long command + compaction ---------------

#[test]
#[ignore = "manual product harness"]
fn s03_long_task() {
    use PlanStepStatus::*;
    let mut s = opened("给 navsvc 加一条端到端的退款审计链路");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated {
            plan: plan(&[
                ("盘点现有 refund 流程", Done),
                ("设计审计事件", Running),
                ("落库", Pending),
                ("补测试", Pending),
            ]),
        }),
    );
    for i in 0..12 {
        tool(
            &mut s,
            &format!("r{i}"),
            "read_file",
            &format!("{{\"path\":\"internal/x{i}.go\"}}"),
            true,
            "package x",
            1,
        );
    }
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::TokenUsage {
            input_tokens: 420_568,
            output_tokens: 16_076,
            cached_input_tokens: 396_288,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ContextCompacted { from: 84, to: 12 }),
    );
    dump(&mut s, "03 long task / after compaction 84 -> 12");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("long"),
            name: "run_command".into(),
            arguments: r#"{"program":"cargo","args":["test","--workspace"]}"#.into(),
            parallel: false,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::CommandProgress {
            label: "cargo test --workspace".into(),
            elapsed_ms: 137_000,
        }),
    );
    dump(&mut s, "03 long task / long command running, 137s elapsed");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("long"),
            ok: true,
            preview: "test result: ok. 3518 passed".into(),
            duration_ms: 268_000,
            applied_diff: None,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AgentActivity {
            label: "等待模型".into(),
        }),
    );
    dump(
        &mut s,
        "03 long task / waiting on the model after a 268s command",
    );
}

// --- 10. resume -----------------------------------------------------------

#[test]
#[ignore = "manual product harness"]
fn s10_resume() {
    use PlanStepStatus::*;
    // A resumed session arrives as one snapshot carrying prior history.
    let mut s = opened("继续之前的退款审计任务");
    let snap = UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "~/Develop/navsvc".into(),
        goal: "给 navsvc 加一条端到端的退款审计链路".into(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/deepseek-v4-flash"),
        mode: leveler_client_protocol::PermissionProfile::Assisted,
        branch: Some("main".into()),
        status: "idle".into(),
        messages: vec![
            UiMessage {
                id: MessageId::new("u1"),
                role: UiRole::User,
                text: "给 navsvc 加一条端到端的退款审计链路".into(),
                ordinal: Some(1),
            },
            UiMessage {
                id: MessageId::new("a1"),
                role: UiRole::Assistant,
                text: "已经落了审计事件结构，还差落库和测试。".into(),
                ordinal: Some(2),
            },
        ],
        pending_interactions: Vec::new(),
        available_models: Vec::new(),
        vision: false,
        last_sequence: Some(84),
        active_tools: Vec::new(),
        plan: Some(plan(&[
            ("盘点现有 refund 流程", Done),
            ("设计审计事件", Done),
            ("落库", Pending),
            ("补测试", Pending),
        ])),
        verification: None,
        diff: Some(UiDiff {
            files: vec![UiDiffFile {
                path: "internal/audit/event.go".into(),
                added: 61,
                removed: 0,
                patch: None,
            }],
        }),
        checkpoints: Vec::new(),
        recaps: Vec::new(),
        user_shells: Vec::new(),
        completion_report: None,
        reasoning: None,
        work_profile: None,
        collaboration: None,
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    dump(
        &mut s,
        "10 resume / reopened with prior plan, diff and history",
    );
}

// --- diagnostics ----------------------------------------------------------

#[test]
#[ignore = "manual product harness"]
fn d01_which_tools_reach_the_activity_stream() {
    let mut s = opened("诊断：每种工具在活动流里如何呈现");
    say(&mut s, "a1", "开始。");
    tool(
        &mut s,
        "t1",
        "read_file",
        "{\"path\":\"a.rs\"}",
        true,
        "x",
        1,
    );
    tool(
        &mut s,
        "t2",
        "list_files",
        r#"{"max_depth":3,"path":"."}"#,
        true,
        "a.rs",
        17,
    );
    tool(
        &mut s,
        "t3",
        "grep",
        "{\"pattern\":\"foo\"}",
        true,
        "a.rs:1",
        16,
    );
    tool(
        &mut s,
        "t4",
        "glob",
        "{\"pattern\":\"**/*.rs\"}",
        true,
        "a.rs",
        5,
    );
    tool(&mut s, "t5", "git_status", "{}", true, "clean", 23);
    tool(
        &mut s,
        "t6",
        "run_command",
        r#"{"program":"echo","args":["hi"]}"#,
        true,
        "hi",
        30,
    );
    tool(
        &mut s,
        "t7",
        "update_goal",
        r#"{"status":"complete","summary":"done"}"#,
        true,
        "",
        0,
    );
    dump(&mut s, "D01 one call of each tool kind");
}

#[test]
#[ignore = "manual product harness"]
fn d02_applied_diff_line_count() {
    let mut s = opened("诊断：编辑行数是否来自 applied diff");
    // 4 added, 1 removed.
    edit(
        &mut s,
        "p1",
        "src/lib.rs",
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -3,6 +3,9 @@\n-pub fn first_even(v: &[i32]) -> i32 {\n+pub fn first_even(v: &[i32]) -> Option<i32> {\n+    if v.is_empty() {\n+        return None;\n+    }\n",
    );
    dump(&mut s, "D02 applied diff with 4 added / 1 removed");
    // An edit whose location could not be established: well-formed patch
    // arguments, but the tool reported no applied diff. The protocol says the
    // UI must then show no line numbers rather than invent one.
    reduce(&mut s, Action::Runtime(RuntimeEvent::ToolCallStarted {
        id: ToolCallId::new("p2"), name: "apply_patch".into(),
        arguments: serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: src/other.rs\n@@\n-let a = 1;\n+let a = 2;\n*** End Patch"
        }).to_string(),
        parallel: false,
    }));
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("p2"),
            ok: true,
            preview: "patched src/other.rs".into(),
            duration_ms: 7,
            applied_diff: None,
        }),
    );
    dump(
        &mut s,
        "D02 second edit with NO applied diff — must not invent a line count",
    );
}

#[test]
#[ignore = "manual product harness"]
fn d03_plan_viewport_under_pressure() {
    use PlanStepStatus::*;
    let mut s = opened("12 步计划，终端很矮");
    let steps: Vec<(&str, PlanStepStatus)> = vec![
        ("步骤一 盘点", Done),
        ("步骤二 设计", Done),
        ("步骤三 建表", Done),
        ("步骤四 写入", Done),
        ("步骤五 读取", Done),
        ("步骤六 缓存", Done),
        ("步骤七 迁移", Done),
        ("步骤八 回填", Running),
        ("步骤九 校验", Pending),
        ("步骤十 文档", Pending),
        ("步骤十一 压测", Pending),
        ("步骤十二 发布", Pending),
    ];
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::PlanUpdated { plan: plan(&steps) }),
    );
    say(&mut s, "a1", "正在回填。");
    for (label, h) in [
        ("24 rows", 24u16),
        ("18 rows", 18),
        ("14 rows", 14),
        ("11 rows", 11),
    ] {
        let backend = TestBackend::new(W, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &mut s)).unwrap();
        let buf = term.backend().buffer();
        println!("\n===== D03 plan viewport at {label} =====");
        for y in 0..h {
            let mut line = String::new();
            let mut x = 0u16;
            while x < W {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                line.push_str(sym);
                x += unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            }
            let l = line.trim_end();
            if !l.is_empty() {
                println!("|{l}");
            }
        }
    }
}
