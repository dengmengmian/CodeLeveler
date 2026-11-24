//! P4 document/detail content contracts.

use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use leveler_client_protocol::{
    DiagnosticCategory, DiagnosticFinding, DiagnosticReport, DiagnosticSeverity, DiagnosticStatus,
    PlanStepStatus, RuntimeStatus, SessionId, UiPlan, UiPlanState, UiPlanStep, UiReviewConclusion,
    UiReviewConfidence, UiReviewCounts, UiReviewCoverage, UiReviewCoverageStatus,
    UiReviewDimension, UiReviewFinding, UiReviewFindingDisposition, UiReviewLimitation,
    UiReviewLimitationKind, UiReviewLimitationSource, UiReviewObject, UiReviewReport,
    UiReviewReportSource, UiReviewScope, UiReviewSeverity, UiReviewStatus, UiReviewSummary,
    UiReviewUsage, UiReviewVerdict,
};
use leveler_tui::action::Action;
use leveler_tui::active_goal::ActiveGoal;
use leveler_tui::output_view::{OutputIndex, OutputMeta, OutputSource, OutputView};
use leveler_tui::reducer::reduce;
use leveler_tui::render::render;
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn opened() -> AppState {
    AppState::new(
        Theme::dark(),
        Boot {
            session_id: SessionId::new("p4"),
            user: "u".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
            untrusted_config: Vec::new(),
            model_notice: None,
            thinking: None,
        },
    )
}

fn render_text(state: &mut AppState, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, state)).unwrap();
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut x = 0;
        while x < buf.area.width {
            let symbol = buf.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(" ");
            out.push_str(symbol);
            x += unicode_width::UnicodeWidthStr::width(symbol).max(1) as u16;
        }
        out.push('\n');
    }
    out
}

fn compact(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn plan() -> UiPlan {
    UiPlan {
        steps: vec![
            UiPlanStep {
                index: 0,
                description: "读取项目结构与现有契约".into(),
                status: PlanStepStatus::Done,
            },
            UiPlanStep {
                index: 1,
                description: "检查跨模块依赖和恢复路径是否仍然正确".into(),
                status: PlanStepStatus::Running,
            },
            UiPlanStep {
                index: 2,
                description: "输出检查报告并验证失败分支".into(),
                status: PlanStepStatus::Pending,
            },
        ],
        state: UiPlanState::Active,
    }
}

#[test]
fn plan_page_shows_progress_current_step_and_every_step() {
    let mut s = opened();
    s.active_screen = Screen::Plan;
    s.status = RuntimeStatus::Busy;
    s.plan = Some(plan());
    let text = compact(&render_text(&mut s, 100, 30));
    for needle in [
        "计划·已完成1/3",
        "读取项目结构与现有契约",
        "检查跨模块依赖和恢复路径是否仍然正确",
        "输出检查报告并验证失败分支",
        "当前步骤",
    ] {
        assert!(text.contains(&compact(needle)), "missing {needle}: {text}");
    }

    let narrow = render_text(&mut s, 60, 20);
    assert!(
        narrow
            .lines()
            .all(|line| { unicode_width::UnicodeWidthStr::width(line) <= 60 }),
        "a Plan row crossed the content width:\n{narrow}"
    );
}

#[test]
fn goal_page_projects_objective_phase_and_plan_without_invented_budget() {
    let mut s = opened();
    s.active_screen = Screen::Goal;
    s.status = RuntimeStatus::Busy;
    s.active_goal = Some(ActiveGoal::running_with_objective(
        "检查仓库".into(),
        "完成当前仓库的结构检查并输出报告。".into(),
        Instant::now(),
    ));
    s.plan = Some(plan());
    let text = compact(&render_text(&mut s, 100, 30));
    for needle in [
        "目标",
        "完成当前仓库的结构检查并输出报告。",
        "执行状态",
        "状态",
        "执行中",
        "耗时",
        "计划",
        "已完成1/3",
        "检查跨模块依赖和恢复路径是否仍然正确",
    ] {
        assert!(text.contains(&compact(needle)), "missing {needle}: {text}");
    }
    for fake in ["预算", "剩余", "Token", "百分比", "100%"] {
        assert!(!text.contains(fake), "invented goal fact {fake}: {text}");
    }
}

fn review_fixture() -> UiReviewReport {
    let summary = UiReviewSummary {
        id: "review-p4".into(),
        session_id: SessionId::new("p4"),
        focus: "检查恢复路径与错误分支".into(),
        scope: UiReviewScope::Workspace,
        model: "deepseek-v4.1".into(),
        status: UiReviewStatus::Completed,
        created_at: "2026-11-23T00:00:00Z".into(),
        finished_at: Some("2026-11-23T00:08:00Z".into()),
        counts: UiReviewCounts {
            p1: 1,
            p3: 1,
            ..Default::default()
        },
        conclusion: Some(UiReviewConclusion::IssuesFound),
        object_changed: false,
        modified_source: false,
        failure: None,
        stop_detail: None,
        usage: UiReviewUsage::default(),
        investigations: Vec::new(),
        report_source: UiReviewReportSource::Submitted,
        report_evidence_revision: 2,
        evidence_revision: 2,
        report_revision: 1,
        report_stale: false,
    };
    UiReviewReport {
        summary,
        object: UiReviewObject {
            workspace: Some("/repo".into()),
            head: Some("abc123".into()),
            revision: Some("HEAD".into()),
            dirty: true,
            ..Default::default()
        },
        findings: vec![
            UiReviewFinding {
                id: "f-1".into(),
                title: "恢复路径丢失状态".into(),
                summary: "恢复路径没有保留 owner".into(),
                dimension: Some(UiReviewDimension::Code),
                severity: Some(UiReviewSeverity::P1),
                confidence: Some(UiReviewConfidence::High),
                kind: "correctness".into(),
                source_child: String::new(),
                file: Some("src/session.rs".into()),
                line: Some(88),
                symbol: None,
                evidence: Some("session.rs:88 的分支直接返回".into()),
                impact: Some("恢复后的会话遗漏 owner".into()),
                recommendation: Some("在恢复前重新绑定 owner".into()),
                reproduced: true,
                disposition: None,
                disposition_reason: None,
                duplicate_of: None,
                source_finding_id: None,
            },
            UiReviewFinding {
                id: "f-2".into(),
                title: "低严重度观察".into(),
                summary: "错误分支缺少回归测试".into(),
                dimension: Some(UiReviewDimension::Test),
                severity: Some(UiReviewSeverity::P3),
                confidence: Some(UiReviewConfidence::Medium),
                kind: "test".into(),
                source_child: String::new(),
                file: None,
                line: None,
                symbol: None,
                evidence: Some("测试列表里没有 error branch".into()),
                impact: None,
                recommendation: None,
                reproduced: false,
                disposition: None,
                disposition_reason: None,
                duplicate_of: None,
                source_finding_id: None,
            },
            UiReviewFinding {
                id: "f-candidate".into(),
                title: "待裁决候选".into(),
                summary: "候选证据".into(),
                dimension: None,
                severity: Some(UiReviewSeverity::P0),
                confidence: None,
                kind: "risk".into(),
                source_child: String::new(),
                file: None,
                line: None,
                symbol: None,
                evidence: None,
                impact: None,
                recommendation: None,
                reproduced: false,
                disposition: Some(UiReviewFindingDisposition::Rejected),
                disposition_reason: Some("没有复现证据".into()),
                duplicate_of: None,
                source_finding_id: None,
            },
        ],
        verdict: UiReviewVerdict {
            conclusion: Some(UiReviewConclusion::IssuesFound),
            coverage: vec![UiReviewCoverage {
                dimension: UiReviewDimension::Code,
                status: UiReviewCoverageStatus::Reviewed,
                scope: Some("src".into()),
                evidence_refs: vec!["session.rs".into()],
                limitations: None,
            }],
            limitations: vec![UiReviewLimitation {
                source: UiReviewLimitationSource::Runtime,
                kind: UiReviewLimitationKind::Environment,
                detail: "没有可用的浏览器环境".into(),
            }],
        },
        report: "# 正式报告\n\n完整正文不能丢失。".into(),
        final_message: String::new(),
    }
}

#[test]
fn review_page_keeps_every_finding_evidence_and_section() {
    let mut s = opened();
    s.active_screen = Screen::Review;
    s.review_report = Some(review_fixture());
    let mut collected = String::new();
    for offset in [0usize, 20, 40, 60, 80, usize::MAX] {
        s.screen_scroll = offset;
        collected.push_str(&render_text(&mut s, 120, 40));
        collected.push('\n');
    }
    let text = compact(&collected);
    for needle in [
        "结论",
        "覆盖",
        "限制",
        "问题",
        "恢复路径丢失状态",
        "低严重度观察",
        "session.rs:88",
        "测试列表里没有errorbranch",
        "P1",
        "P3",
        "待裁决候选",
        "没有复现证据",
        "没有可用的浏览器环境",
        "正式报告",
        "完整正文不能丢失。",
    ] {
        assert!(text.contains(&compact(needle)), "missing {needle}: {text}");
    }
    let narrow = render_text(&mut s, 60, 20);
    assert!(
        narrow
            .lines()
            .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 60),
        "a Review row crossed the content width:\n{narrow}"
    );
}

#[test]
fn document_home_and_end_reach_the_first_and_last_plan_step() {
    let mut s = opened();
    s.active_screen = Screen::Plan;
    s.status = RuntimeStatus::Busy;
    s.plan = Some(UiPlan {
        steps: (0..20)
            .map(|index| UiPlanStep {
                index,
                description: format!("step-{index}"),
                status: if index == 0 {
                    PlanStepStatus::Running
                } else {
                    PlanStepStatus::Pending
                },
            })
            .collect(),
        state: UiPlanState::Active,
    });
    let _ = render_text(&mut s, 60, 20);
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::End, KeyModifiers::empty())),
    );
    let end = compact(&render_text(&mut s, 60, 20));
    assert!(end.contains("step-19"), "{end}");
    reduce(
        &mut s,
        Action::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::empty())),
    );
    let home = compact(&render_text(&mut s, 60, 20));
    assert!(home.contains("step-0"), "{home}");
    assert!(!home.contains("step-19"), "{home}");
}

#[test]
fn output_index_is_built_for_the_hosted_page_content_width() {
    let mut s = opened();
    s.active_screen = Screen::Output;
    let source = OutputSource::Memory(Arc::from(
        (0..40)
            .map(|index| format!("output-{index}\n"))
            .collect::<String>(),
    ));
    // The 120-column hosted Viewer content is 102 cells wide (card border,
    // tabs and page gutter included). The viewer must not spend every frame
    // saying the window changed.
    let index = Arc::new(OutputIndex::build(&source, 102).unwrap());
    let mut view = OutputView::new(source, "seq 1 40".into(), 102);
    view.index = Some(index);
    view.loading = false;
    s.output_view = Some(view);
    let text = compact(&render_text(&mut s, 120, 40));
    assert!(text.contains("output-39"), "{text}");
    for stale in ["重新排版", "reflow"] {
        assert!(!text.contains(stale), "the page stayed in reflow: {text}");
    }
}

fn long_evidence() -> Vec<String> {
    (0..40)
        .map(|index| format!("evidence-line-{index}: 连接失败后没有恢复状态"))
        .collect()
}

#[test]
fn doctor_page_separates_checks_and_scrolls_the_evidence_to_its_end() {
    let mut s = opened();
    s.active_screen = Screen::Doctor;
    s.doctor.report = Some(DiagnosticReport {
        generated_at: "2026-11-23T00:00:00Z".into(),
        findings: vec![DiagnosticFinding {
            id: "custom-check".into(),
            subject: None,
            category: DiagnosticCategory::Mcp,
            severity: DiagnosticSeverity::Error,
            status: DiagnosticStatus::Fail,
            evidence: long_evidence(),
            action: None,
            checked_at: "2026-11-23T00:00:00Z".into(),
        }],
    });
    // First screen: the overview and the checks section, before a selected
    // row scrolls the list to its own line.
    let overview = compact(&render_text(&mut s, 100, 40));
    for needle in ["检查领域", "检查项", "证据", "失败"] {
        assert!(
            overview.contains(&compact(needle)),
            "missing {needle}: {overview}"
        );
    }

    s.doctor.selected = Some("custom-check".into());
    s.doctor.detail_open = true;
    let top = compact(&render_text(&mut s, 100, 30));
    for needle in [
        "检查领域",
        "检查项",
        "状态",
        "类别",
        "证据",
        "evidence-line-0",
    ] {
        assert!(top.contains(&compact(needle)), "missing {needle}: {top}");
    }
    assert!(
        !top.contains("evidence-line-39"),
        "the detail was clamped before End"
    );
    s.doctor.detail_scroll = usize::MAX;
    let end = compact(&render_text(&mut s, 100, 30));
    assert!(
        end.contains("evidence-line-39"),
        "the detail did not reach its last line: {end}"
    );
}

#[test]
fn output_page_has_a_real_empty_state_and_real_metadata_header() {
    let mut s = opened();
    s.active_screen = Screen::Output;
    s.output_view = None;
    let empty = compact(&render_text(&mut s, 80, 24));
    assert!(empty.contains("没有可显示的输出"), "{empty}");

    let source = OutputSource::Memory(Arc::from("alpha\nbeta\n"));
    let mut view = OutputView::new(source, "seq 1 200".into(), 80);
    view.loading = false;
    view.meta = OutputMeta {
        exit_code: Some(0),
        duration_ms: Some(2000),
        has_stderr: true,
    };
    s.output_view = Some(view);
    let text = compact(&render_text(&mut s, 120, 30));
    for needle in [
        "命令seq1200",
        "exitcode0",
        "2s",
        "含stderr",
        "alpha",
        "beta",
    ] {
        assert!(text.contains(&compact(needle)), "missing {needle}: {text}");
    }
}
