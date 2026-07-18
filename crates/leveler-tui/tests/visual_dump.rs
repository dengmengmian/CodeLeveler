//! TEMP: headless render of a realistic conversation, to eyeball display bugs.
use leveler_client_protocol::{MessageId, RuntimeEvent, SessionId, ToolCallId, UiSessionSnapshot};
use leveler_tui::action::Action;
use leveler_tui::reducer::reduce;
use leveler_tui::render::{item_render, render};
use leveler_tui::screen::Screen;
use leveler_tui::state::{AppState, Boot};
use leveler_tui::theme::Theme;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn screen_dump(state: &mut AppState, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| render(f, state)).unwrap();
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let mut x = 0u16;
        while x < w {
            let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(sym);
            let cw = unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            x += cw;
        }
        out.push('\n');
    }
    out
}

fn line_text(l: &ratatui::text::Line) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn opened() -> AppState {
    let mut s = AppState::new(
        Theme::no_color(),
        Boot {
            session_id: SessionId::new("s1"),
            user: "麻凡".into(),
            version: "0.1.0".into(),
            show_welcome: false,
            draft_path: None,
            history_path: None,
            context_window: 200_000,
            locale: leveler_tui::Locale::Zh,
        },
    );
    let snap = UiSessionSnapshot {
        id: SessionId::new("s1"),
        repository: "~/x".into(),
        goal: "g".into(),
        model: leveler_client_protocol::ModelRef::parse("deepseek/v3"),
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
        completion_report: None,
    };
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SessionOpened { session: snap }),
    );
    s
}

#[test]
fn agents_screen_lists_spawned_sub_agents_in_direct_mode() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "agent-1".into(),
            nickname: "Newton".into(),
            role: "explorer".into(),
            done: false,
            ok: false,
            detail: "investigating the parser".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentProgress {
            id: "agent-1".into(),
            active: true,
            input_tokens: 1_000,
            output_tokens: 50,
            cached_input_tokens: 500,
        }),
    );
    s.active_screen = Screen::Agents;
    let screen = screen_dump(&mut s, 80, 20);
    assert!(
        screen.contains("探索 Agent 1 · 执行中")
            && !screen.contains("Newton")
            && !screen.contains("[explorer]"),
        "the Agents screen must list spawned sub-agents:\n{screen}"
    );
    assert!(
        screen.contains("子 Agent"),
        "with a sub-agent section header"
    );
}

/// Manual visual-inspection harness — renders a realistic conversation + the
/// full-screen views to text so display bugs (overflow, wrapping, alignment,
/// control chars) can be eyeballed headlessly. Not an assertion; run on demand:
///   cargo test -p leveler-tui --test visual_dump -- --ignored --nocapture
#[test]
#[ignore = "manual visual harness; run with --ignored --nocapture"]
fn visual_inspect() {
    let mut s = opened();
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::UserMessageAdded {
            message: leveler_client_protocol::UiMessage {
                id: MessageId::new("u1"),
                role: leveler_client_protocol::UiRole::User,
                text: "分析一下 usage resolver 的设计，给个对比表和代码示例".into(),
            },
        }),
    );
    let md = "## Usage Resolver 设计\n\n\
        它把不同 provider 的 token 用量**归一化**到统一结构。核心在 `resolveUsage()`，见 `resolver.go:120`。\n\n\
        | Provider | 字段 | 归一化 |\n|---|---|---|\n\
        | OpenAI | `usage.total_tokens` | 直接取 |\n\
        | DeepSeek | `usage.prompt_cache_hit_tokens` | 需要合并 cache 命中 🔴 |\n\
        | 智谱 GLM | `usage.prompt_tokens` | 兼容层转换 |\n\n\
        示例(注意这行故意很长来测试代码块换行是否正确处理超出终端宽度的情况):\n\n\
        ```go\nfunc resolveUsage(raw map[string]any, provider string) (Usage, error) { return normalize(raw), nil } // 这是一个很长很长的注释确保超宽\n```\n\n\
        要点:\n- 每个 provider 一个 adapter\n- 失败回退到零值\n- 支持 cache 命中统计\n";
    let id = MessageId::new("a1");
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageStarted {
            message_id: id.clone(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantTextDelta {
            message_id: id.clone(),
            delta: md.into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::AssistantMessageCompleted {
            message_id: id.clone(),
        }),
    );

    // A couple of tool blocks + a sub-agent.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallStarted {
            id: ToolCallId::new("t1"),
            name: "grep".into(),
            arguments: "{\"pattern\":\"resolveUsage\"}".into(),
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::ToolCallCompleted {
            id: ToolCallId::new("t1"),
            ok: true,
            preview: "resolver.go:120\tfunc resolveUsage(".into(),
            duration_ms: 12,
        }),
    );
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::SubAgentUpdated {
            id: "ag1".into(),
            nickname: "Newton".into(),
            role: "explorer".into(),
            done: true,
            ok: true,
            detail: "resolver.go 定义了 4 个 provider adapter".into(),
        }),
    );

    // Full-screen views.
    reduce(
        &mut s,
        Action::Runtime(RuntimeEvent::DiffUpdated {
            diff: leveler_client_protocol::UiDiff {
                files: (0..8)
                    .map(|i| leveler_client_protocol::UiDiffFile {
                        path: format!("internal/llm/adapter_{i}.go"),
                        added: i * 3,
                        removed: i,
                        patch: Some(format!(
                            "@@ -1 +1 @@\n-old line {i}\n+new line {i} 一些中文改动内容\n {}",
                            "x".repeat(90)
                        )),
                    })
                    .collect(),
            },
        }),
    );
    for (name, scr) in [
        ("Diff", Screen::Diff),
        ("Tools", Screen::Tools),
        ("Agents", Screen::Agents),
    ] {
        s.active_screen = scr;
        println!("\n╔══════ {name} screen 70x18 ══════╗");
        print!("{}", screen_dump(&mut s, 70, 18));
    }
    s.active_screen = Screen::Conversation;
    println!("\n╔══════ Conversation full screen 80x24 ══════╗");
    print!("{}", screen_dump(&mut s, 80, 24));

    for width in [96usize, 46] {
        println!("\n══════════ width {width} ══════════");
        for item in s.transcript.items() {
            for line in item_render(item, &s.theme, width, false, s.t()) {
                let t = line_text(&line);
                let w: usize = line
                    .spans
                    .iter()
                    .map(|sp| unicode_width::UnicodeWidthStr::width(sp.content.as_ref()))
                    .sum();
                let over = if w > width {
                    format!("  <<OVER {w}>>")
                } else {
                    String::new()
                };
                println!("|{t}{over}");
            }
            println!("|  ---");
        }
    }
}
