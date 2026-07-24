//! UI locale and string tables.
//!
//! Resolution (highest first):
//! 1. `LEVELER_LANG=en|zh` (process env override)
//! 2. `lang` in `~/.leveler/config.toml`
//! 3. `LC_ALL` / `LC_MESSAGES` / `LANG` (system)
//! 4. Default Chinese
//!
//! Only English systems auto-select English; everything else (including unknown
//! and `C`/`POSIX`) falls back to Chinese. Call [`Locale::resolve`] once at the
//! composition root and pass it through [`crate::Boot`] — do not re-read env
//! per frame.

/// Supported UI languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    #[default]
    Zh,
    En,
}

impl Locale {
    /// Resolve UI language: `LEVELER_LANG` → optional config `lang` → system → zh.
    pub fn resolve(config_lang: Option<&str>) -> Self {
        if let Some(raw) = leveler_core::environment().var("LEVELER_LANG") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Self::parse_override(trimmed);
            }
        }
        if let Some(raw) = config_lang.map(str::trim).filter(|s| !s.is_empty()) {
            return Self::parse_override(raw);
        }
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Some(raw) = leveler_core::environment().var(key)
                && let Some(loc) = Self::from_locale_tag(&raw)
            {
                return loc;
            }
        }
        Self::Zh
    }

    /// Resolve from the process environment only (no config file).
    pub fn from_env() -> Self {
        Self::resolve(None)
    }

    /// `LEVELER_LANG` values: `en`, `en_US`, `zh`, `zh-CN`, …
    /// Non-English overrides still map to Chinese (product default).
    pub fn parse_override(raw: &str) -> Self {
        let s = raw.trim().to_ascii_lowercase();
        if s.is_empty() {
            return Self::Zh;
        }
        if primary_lang(&s).as_deref() == Some("en") {
            Self::En
        } else {
            Self::Zh
        }
    }

    /// Parse a POSIX/BCP47 locale tag. `C`/`POSIX`/empty → `None` (keep looking).
    pub fn from_locale_tag(raw: &str) -> Option<Self> {
        let s = raw.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("C") || s.eq_ignore_ascii_case("POSIX") {
            return None;
        }
        match primary_lang(s)?.as_str() {
            "en" => Some(Self::En),
            "zh" => Some(Self::Zh),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    pub fn text(self) -> &'static UiText {
        match self {
            Self::Zh => &ZH,
            Self::En => &EN,
        }
    }
}

fn primary_lang(tag: &str) -> Option<String> {
    let base = tag.split(['.', '@']).next()?.trim();
    if base.is_empty() {
        return None;
    }
    Some(base.split(['_', '-']).next()?.trim().to_ascii_lowercase())
}

/// Static UI copy for one locale. Prefer methods for formatted strings.
#[derive(Debug)]
pub struct UiText {
    // Permissions / chrome
    pub perm_readonly: &'static str,
    pub perm_full: &'static str,
    pub perm_workspace: &'static str,
    pub send: &'static str,
    pub newline: &'static str,
    pub composer_placeholder: &'static str,
    pub cancel: &'static str,
    pub quit: &'static str,
    pub jump_bottom: &'static str,
    pub waiting_model: &'static str,
    pub goal_mode: &'static str,
    pub ctrl_c_cancel: &'static str,
    pub waiting_reply: &'static str,
    pub back_to_bottom: &'static str,

    // Context gauge
    pub context_label: &'static str,
    pub context_tokens_only: &'static str, // "上下文 {} tokens" / "context {} tokens"
    pub context_empty_window: &'static str, // "上下文 — / {} tokens"
    pub context_with_bar: &'static str,    // "上下文 {bar} {} / {}"
    pub cached_pct: &'static str,          // " ({}% 缓存)"
    pub suggest_compact: &'static str,
    pub compact_hint: &'static str,

    // Turn end
    pub turn_completed: &'static str,
    pub turn_answered: &'static str,
    pub turn_truncated: &'static str,
    pub turn_incomplete: &'static str,
    pub turn_unverified: &'static str,
    pub turn_no_automatic_verification: &'static str,
    /// No VCS-tracked edits this turn — calm complete marker, not "unverified".
    pub turn_no_code_changes: &'static str,
    pub turn_failed: &'static str,
    pub turn_cancelled: &'static str,
    /// Short incomplete reasons (localized machine tokens / long defaults).
    pub turn_budget_exhausted: &'static str,
    pub turn_stalled_goal: &'static str,
    pub turn_observe_thrash: &'static str,
    pub turn_plan_thrash: &'static str,
    pub tool_calls_n: &'static str, // " · {} 次工具"
    pub recap_label: &'static str,
    pub recap_next_step: &'static str,
    pub sub_agent_default: &'static str,
    pub sub_agent_explorer: &'static str,
    pub sub_agent_worker: &'static str,
    pub sub_agent_waiting: &'static str,
    pub sub_agent_running: &'static str,
    pub sub_agent_completed: &'static str,
    pub sub_agent_incomplete: &'static str,
    pub sub_agent_round_limit: &'static str,
    pub sub_agent_latest_note: &'static str,
    pub sub_agent_task: &'static str,
    pub sub_agent_result: &'static str,
    pub sub_agent_cached: &'static str,
    pub agents_sub_agents: &'static str,
    pub agents_orchestrator: &'static str,
    pub agents_idle: &'static str,
    pub agents_empty: &'static str,
    pub agents_scroll_hint: &'static str,
    pub unsupported_task_action: &'static str,
    pub unsupported_task_hint: &'static str,
    /// Shown instead of the internal validation error when an update_plan is
    /// rejected (e.g. skipping an unfinished step).
    pub plan_update_rejected: &'static str,
    /// Shown when a read-only/observe call is turned down by a guard (closeout,
    /// loop-guard) — the model gets English guidance; the user sees this.
    pub observe_denied: &'static str,
    pub tool_group_calls: &'static str,
    pub tool_group_running: &'static str,
    pub tool_group_succeeded: &'static str,
    pub tool_group_adjustment: &'static str,
    pub tool_group_expand: &'static str,
    pub tool_group_collapse: &'static str,
    pub tool_details: &'static str,
    pub tool_status_running: &'static str,
    pub tool_status_succeeded: &'static str,
    pub tool_status_failed: &'static str,
    pub tool_output_lines: &'static str,
    /// Aggregated exploration: "已检查项目结构（读取 5 · 搜索 2）"
    pub activity_explored: &'static str,
    pub activity_reads: &'static str,    // "读取 {}"
    pub activity_searches: &'static str, // "搜索 {}"
    pub activity_symbols: &'static str,  // "符号 {}"
    /// Search-heavy aggregate: "找到 {} 个相关代码位置"
    pub activity_found_locations: &'static str,
    pub file_mention: &'static str,

    // Welcome / help shell
    pub welcome_back: &'static str, // "欢迎回来，{}"
    /// Empty Conversation splash tagline.
    pub splash_tagline: &'static str,
    /// Empty Conversation one-line hint.
    pub splash_hint: &'static str,
    /// Splash "getting started" column heading.
    pub splash_tips_title: &'static str,
    /// Splash lead line above the command list.
    pub splash_tips_lead: &'static str,
    pub branch_label: &'static str,
    pub help_commands: &'static str,
    pub help_keys: &'static str,
    pub help_view_all: &'static str,
    pub help_scroll: &'static str,
    pub help_title: &'static str,
    pub key_submit: &'static str,
    pub key_newline: &'static str,
    pub key_cancel_quit: &'static str,
    pub key_screens: &'static str,
    pub key_expand: &'static str,
    pub key_turn_nav: &'static str,
    pub key_model: &'static str,
    pub key_jump: &'static str,
    pub key_end: &'static str,
    pub key_tab: &'static str,
    pub key_esc: &'static str,
    /// "轮次 {}/{}" / "turn {}/{}"
    pub turn_nav: &'static str,
    /// Back to live edge after turn review.
    pub turn_nav_live: &'static str,
    /// No user turns yet.
    pub turn_nav_empty: &'static str,

    // Screens
    pub screen_context: &'static str,
    pub screen_sessions: &'static str,
    pub screen_tools: &'static str,
    pub screen_plan: &'static str,
    pub screen_diff: &'static str,
    pub screen_verify: &'static str,
    pub screen_agents: &'static str,
    pub no_sessions: &'static str,
    pub no_context: &'static str,
    pub no_plan: &'static str,
    /// Label fragment for "step k of n" in sticky plan header (e.g. "步").
    pub plan_step_of: &'static str,
    pub no_diff: &'static str,
    pub no_verify: &'static str,
    pub active_plan: &'static str,
    pub active_rules: &'static str,
    pub estimated_tokens: &'static str,
    pub candidate_files: &'static str,

    // Reasoning / queue footer
    pub thinking: &'static str,
    pub thinking_lines: &'static str, // " · {} 行"
    pub expand_thinking_tools: &'static str,
    pub queue_retry: &'static str,
    pub queue_sending: &'static str,
    pub queue_waiting: &'static str,
    pub queue_backspace: &'static str,
    /// One-line footer: "队列 {n}" / "queue {n}"
    pub queue_count: &'static str,
    /// "（含 {n} 待重试）" / " ({n} retry)"
    pub queue_retry_n: &'static str,
    /// "下一条：" / "next: "
    pub queue_next: &'static str,
    /// Short backspace hint on the same line.
    pub queue_del_hint: &'static str,
    /// Action hint under the expanded queue when items are waiting.
    pub queue_actions_hint: &'static str,
    /// Notice when "start now" interrupts the running turn.
    pub queue_starting_now: &'static str,

    // Slash command descriptions (same order as screen::SLASH_COMMANDS)
    pub slash: SlashText,
    /// Ghost argument placeholders drawn after the caret (not in the buffer).
    pub slash_ghost: SlashGhost,

    // Notifications / mode
    pub queued_n: &'static str,
    pub cleared_queue: &'static str,
    pub deleted_queue_n: &'static str,
    pub theme_dark: &'static str,
    pub theme_light: &'static str,
    pub theme_ion_label: &'static str,
    pub theme_ion_desc: &'static str,
    pub theme_night_label: &'static str,
    pub theme_night_desc: &'static str,
    pub theme_day_label: &'static str,
    pub theme_day_desc: &'static str,
    pub mode_workflow_on: &'static str,
    pub mode_workflow_off: &'static str,
    pub cancelled_continue: &'static str,
    pub mode_plan_desc: &'static str,
    pub mode_write_desc: &'static str,
    pub mode_full_desc: &'static str,
    pub overlay_approval: &'static str,
    pub overlay_clarify: &'static str,
    pub overlay_model: &'static str,
    pub overlay_mode: &'static str,
    pub overlay_theme: &'static str,
    pub overlay_media: &'static str,
    pub overlay_checkpoint: &'static str,
    pub pending_n: &'static str, // "+{n} pending" style short for busy status
    pub btw_label: &'static str,
    pub btw_q: &'static str,
    pub btw_a: &'static str,
    pub btw_usage: &'static str,
    pub btw_failed: &'static str,
    /// In-flight status inside the /btw floating card.
    pub btw_answering: &'static str,
    /// Footer/card hint: how to close a finished 旁问 card.
    pub btw_dismiss: &'static str,

    // Final status (turn closeout)
    pub final_completed: &'static str,
    pub final_completed_warnings: &'static str,
    pub final_waiting_confirmation: &'static str,
    pub final_blocked: &'static str,
    /// Terminal marker when the run stopped on a failed verification gate
    /// (e.g. cargo test failed) — a verification failure, not a system block.
    pub final_verification_failed: &'static str,
    pub final_failed: &'static str,
    pub final_cancelled: &'static str,

    // Turn end / completion report
    pub turn_end_completed: &'static str,
    pub completion_files_changed: &'static str, // "修改 {} 个文件"
    pub completion_verified: &'static str,      // "验证 {}/{} 通过"
    pub completion_diff_hint: &'static str,

    // Fold hints
    pub fold_more_lines: &'static str, // "… 还有 {} 行 · Ctrl+O 展开"
    pub fold_more_lines_short: &'static str, // "(+{} 行 · Ctrl+O)"
    pub fold_full_diff: &'static str,

    // Tool detail labels
    pub tool_label_args: &'static str,
    pub tool_label_patch: &'static str,
    pub tool_label_replace: &'static str,
    pub tool_word_blocked: &'static str,
    pub tool_word_done: &'static str,

    // Tools screen
    pub tools_none: &'static str,
    pub tools_col_tool: &'static str,
    pub tools_col_status: &'static str,
    pub tools_status_running: &'static str,
    pub tools_status_ok: &'static str,
    pub tools_status_attention: &'static str,
    pub tools_col_duration: &'static str,
    pub tools_output: &'static str,

    // Parallel batch header
    pub parallel_header: &'static str, // "并行执行 {} 个工具"

    // Sub-agent tree
    pub agents_running_header: &'static str, // "{} 个 agents 正在运行"
    pub agents_done_header: &'static str,    // "{} 个 agents 完成"
    pub agents_ended_header: &'static str,   // "{} 个 agents 结束"
    pub agent_status_running: &'static str,
    pub agent_status_completed: &'static str,
    pub agent_status_timeout: &'static str,

    // Tool result
    pub result_timeout: &'static str,

    // Edit merge summary
    pub edit_merge_summary: &'static str, // "{} 处修改"

    // Tools screen footer hint
    pub tools_footer_hint_full: &'static str,
    pub tools_footer_hint_compact: &'static str,
}

#[derive(Debug)]
pub struct SlashText {
    pub model: &'static str,
    pub mode: &'static str,
    pub goal: &'static str,
    pub btw: &'static str,
    pub workflow: &'static str,
    pub work_mode: &'static str,
    pub collab: &'static str,
    pub plan_collab: &'static str,
    pub confirm_plan: &'static str,
    pub memory: &'static str,
    pub skill: &'static str,
    pub steps: &'static str,
    pub diff: &'static str,
    pub verify: &'static str,
    pub tools: &'static str,
    pub sessions: &'static str,
    pub context: &'static str,
    pub agents: &'static str,
    pub restore: &'static str,
    pub compact: &'static str,
    pub export: &'static str,
    pub web: &'static str,
    pub image: &'static str,
    pub attach: &'static str,
    pub paste: &'static str,
    pub theme: &'static str,
    pub clear: &'static str,
    pub help: &'static str,
    pub quit: &'static str,
}

/// Argument ghosts for commands that require free-text params.
/// `*_spaced` includes a leading space for when the buffer is just `/cmd`.
#[derive(Debug)]
pub struct SlashGhost {
    pub btw: &'static str,
    pub btw_spaced: &'static str,
    pub goal: &'static str,
    pub goal_spaced: &'static str,
    pub path: &'static str,
    pub path_spaced: &'static str,
    pub skill: &'static str,
    pub skill_spaced: &'static str,
}

static ZH: UiText = UiText {
    perm_readonly: "请求批准",
    perm_full: "完全访问（免审批）",
    perm_workspace: "替我审批",
    send: "发送",
    newline: "换行",
    composer_placeholder: "输入消息，/ 查看命令",
    cancel: "取消",
    quit: "退出",
    jump_bottom: "回底",
    waiting_model: "等待模型",
    goal_mode: "目标模式",
    ctrl_c_cancel: "Ctrl+C 取消",
    waiting_reply: "◇ 等待你的回复",
    back_to_bottom: "已回到底部",
    context_label: "上下文",
    context_tokens_only: "上下文 {} tokens",
    context_empty_window: "上下文窗口 {} tokens · 跑一轮后显示用量",
    context_with_bar: "上下文 {} {} / {}",
    cached_pct: " ({}% 缓存)",
    suggest_compact: "建议 /compact",
    compact_hint: "/compact",
    turn_completed: "✓ 任务已完成",
    turn_answered: "✓ 回答结束",
    turn_truncated: "⚠ 输出被截断 · 可继续提问",
    turn_incomplete: "⚠ 任务未完成",
    turn_unverified: "✓ 完成 · 未自动验证",
    turn_no_automatic_verification: "无自动验证配置",
    turn_no_code_changes: "◇ 结束 · 未改仓库",
    turn_failed: "✗ 已停止 · 查看上方错误",
    turn_cancelled: "■ 已停止 · 已取消",
    turn_budget_exhausted: "预算用尽 · 说「继续」或 /goal 接着做",
    turn_stalled_goal: "goal 未确认完成",
    turn_observe_thrash: "无进展 · 重复观察已中止",
    turn_plan_thrash: "计划已完成 · 重复观察已中止",
    tool_calls_n: " · {} 次工具",
    recap_label: "回顾",
    recap_next_step: "下一步：",
    sub_agent_default: "子 Agent",
    sub_agent_explorer: "探索 Agent",
    sub_agent_worker: "执行 Agent",
    sub_agent_waiting: "等待执行",
    sub_agent_running: "执行中",
    sub_agent_completed: "已完成",
    sub_agent_incomplete: "未完成",
    sub_agent_round_limit: "未在 {} 轮内完成。",
    sub_agent_latest_note: "最后进展：",
    sub_agent_task: "任务：",
    sub_agent_result: "结果：",
    sub_agent_cached: "缓存",
    agents_sub_agents: "子 Agent",
    agents_orchestrator: "编排器",
    agents_idle: "空闲",
    agents_empty: "暂无 Agent 活动（spawn_agent 派生子 Agent，或 /workflow 运行后生成）",
    agents_scroll_hint: "↑↓/PgUp/PgDn 滚动 · Esc 返回",
    unsupported_task_action: "委派（不支持）",
    unsupported_task_hint: "不支持 task，请改用 spawn_agent",
    plan_update_rejected: "计划未更新：需按顺序完成步骤",
    observe_denied: "已跳过：重复的检查无需再次执行",
    tool_group_calls: "工具调用 {} 次",
    tool_group_running: "正在{}",
    tool_group_succeeded: "{} 成功",
    tool_group_adjustment: "{} 需调整",
    tool_group_expand: "展开",
    tool_group_collapse: "收起",
    tool_details: "详情",
    tool_status_running: "运行中",
    tool_status_succeeded: "成功",
    tool_status_failed: "失败",
    tool_output_lines: "{} 行",
    activity_explored: "已检查项目结构",
    activity_reads: "读取 {}",
    activity_searches: "搜索 {}",
    activity_symbols: "符号 {}",
    activity_found_locations: "找到 {} 个相关代码位置",
    file_mention: "文件",
    welcome_back: "欢迎回来，{}",
    splash_tagline: "AI Coding Agent · 不同模型 · 同一工程标准",
    splash_hint: "输入任务开始 · / 查看命令 · ↑ 历史",
    splash_tips_title: "上手提示",
    splash_tips_lead: "输入任务直接开始，或用命令：",
    branch_label: "分支 ",
    help_commands: "命令",
    help_keys: "快捷键",
    help_view_all: " 查看全部命令",
    help_scroll: "↑↓/PgUp/PgDn 滚动 · Esc 返回",
    help_title: "帮助",
    key_submit: "提交",
    key_newline: "换行",
    key_cancel_quit: "取消 / 退出",
    key_screens: "步骤/Diff/验证/工具/会话/Agents",
    key_expand: "Ctrl+O：展开/收起当前思考，否则仅最新工具组",
    key_turn_nav: "跳转用户轮次",
    key_model: "切换模型",
    key_jump: "回到底部输入（滚上看历史后）",
    key_end: "空输入时回底；有字时行尾",
    key_tab: "补全命令",
    key_esc: "关闭页面 / 弹层",
    turn_nav: "轮次 {}/{}",
    turn_nav_live: "回到最新",
    turn_nav_empty: "还没有用户消息",
    screen_context: "上下文",
    screen_sessions: "会话",
    screen_tools: "工具",
    screen_plan: "任务步骤",
    screen_diff: "改动",
    screen_verify: "验证结果",
    screen_agents: "Agent 状态",
    no_sessions: "暂无会话",
    no_context: "暂无上下文信息（/workflow 编排工作流运行后生成）",
    no_plan: "暂无任务步骤（/workflow 开启后运行任务会生成）",
    plan_step_of: "当前",
    no_diff: "无改动",
    no_verify: "暂无验证结果",
    active_plan: "计划",
    active_rules: "行为约束",
    estimated_tokens: "估算 token：{}",
    candidate_files: "候选文件（{}）",
    thinking: "思考",
    thinking_lines: " · {} 行",
    expand_thinking_tools: "  … Ctrl+O 展开当前思考（无思考时切换最新工具组）",
    queue_retry: "↻ 待重试",
    queue_sending: "→ 发送中",
    queue_waiting: "⏳ 排队",
    queue_backspace: "  (退格删除最近一条)",
    queue_count: "⏳ 队列 {}",
    queue_retry_n: "（含 {} 待重试）",
    queue_next: "下一条：",
    queue_del_hint: "⌫ 删末条",
    queue_actions_hint: "  Enter 马上开始 · Delete 取消 · Alt+↑↓ 排序",
    queue_starting_now: "打断当前，马上开始这条",
    slash: SlashText {
        model: "切换使用的 AI 模型",
        mode: "权限档：逐步批准 / 辅助放行 / 全权放行",
        goal: "目标模式：设定目标，自动多轮推进",
        btw: "临时提问（不写入主对话）",
        workflow: "执行方式：直接执行 / 分阶段编排",
        work_mode: "工作档 economy 省 / balanced 均衡 / delivery 交付",
        collab: "协作 chat 对话 / plan 方案 / goal 目标",
        plan_collab: "只读分析：先给方案，不改代码",
        confirm_plan: "确认方案并转入目标执行",
        memory: "项目记忆（forget <id> 删除某条）",
        skill: "调用技能（等同输入 $技能名）",
        steps: "查看任务步骤（编排后生成）",
        diff: "查看代码改动",
        verify: "查看验证结果",
        tools: "查看工具调用记录",
        sessions: "历史会话列表",
        context: "上下文用量",
        agents: "多智能体运行状态",
        restore: "回滚到检查点",
        compact: "压缩上下文（节省 token）",
        export: "导出对话为 markdown（默认存当前目录，可 /export <路径>）",
        web: "在浏览器打开 Web UI（loopback + token，同一会话）",
        image: "添加图片附件",
        attach: "添加文件附件",
        paste: "粘贴剪贴板图片",
        theme: "选择配色主题（ion / night / day）",
        clear: "新建对话（清空上下文）",
        help: "查看帮助",
        quit: "退出程序",
    },
    slash_ghost: SlashGhost {
        btw: "<问题>",
        btw_spaced: " <问题>",
        goal: "<任务目标>",
        goal_spaced: " <任务目标>",
        path: "<文件路径>",
        path_spaced: " <文件路径>",
        skill: "<技能名> [任务说明]",
        skill_spaced: " <技能名> [任务说明]",
    },
    queued_n: "已排队 {} 条，将在当前任务后依次运行",
    cleared_queue: "已清空排队",
    deleted_queue_n: "已删除一条排队，剩 {} 条",
    theme_dark: "已切换到暗色主题",
    theme_light: "已切换到亮色主题",
    theme_ion_label: "Ion（默认）",
    theme_ion_desc: "冷青品牌色，深色终端",
    theme_night_label: "Night",
    theme_night_desc: "深蓝紫夜色",
    theme_day_label: "Day",
    theme_day_desc: "浅色，适合亮底终端",
    mode_workflow_on: "已切换到编排工作流（理解 → 计划 → 执行 → 验证）",
    mode_workflow_off: "已切换到直接模式",
    cancelled_continue: "已取消，可继续输入",
    mode_plan_desc: "始终询问：外写文件、用网、危险命令都要你点同意",
    mode_write_desc: "半自动：读写/联网/shell（含 git push）自动执行，仅删除/提权/打开外部应用询问",
    mode_full_desc: "免审批：读写、危险命令、删除、记忆写入全部自动执行",
    overlay_approval: "等待授权",
    overlay_clarify: "Agent 需要澄清",
    overlay_model: "选择模型",
    overlay_mode: "选择权限模式",
    overlay_theme: "选择主题",
    overlay_media: "模型不支持图片",
    overlay_checkpoint: "恢复检查点",
    pending_n: "+{} 待处理",
    btw_label: "临时提问",
    btw_q: "问",
    btw_a: "答",
    btw_usage: "用法: /btw <问题>",
    btw_failed: "临时提问失败",
    btw_answering: "回答中…",
    btw_dismiss: "关闭",
    final_completed: "已完成",
    final_completed_warnings: "已完成，但有警告",
    final_waiting_confirmation: "等待确认",
    final_blocked: "未完成",
    final_verification_failed: "验证未通过",
    final_failed: "失败",
    final_cancelled: "已取消",
    turn_end_completed: "任务已完成",
    completion_files_changed: "修改 {} 个文件",
    completion_verified: "验证 {}/{} 通过",
    completion_diff_hint: "/diff 查看改动",
    fold_more_lines: "… 还有 {} 行 · Ctrl+O 展开",
    fold_more_lines_short: "(+{} 行 · Ctrl+O)",
    fold_full_diff: "… Ctrl+O 查看完整 Diff",
    tool_label_args: "参数",
    tool_label_patch: "补丁",
    tool_label_replace: "文本替换",
    tool_word_blocked: "受阻",
    tool_word_done: "完成",
    tools_none: "无工具调用",
    tools_col_tool: "工具",
    tools_col_status: "状态",
    tools_status_running: "运行中",
    tools_status_ok: "成功",
    tools_status_attention: "需调整",
    tools_col_duration: "耗时",
    tools_output: "输出",
    parallel_header: "并行执行 {} 个工具",
    agents_running_header: "{} 个 agents 正在运行",
    agents_done_header: "{} 个 agents 完成",
    agents_ended_header: "{} 个 agents 结束",
    agent_status_running: "进行中",
    agent_status_completed: "已完成",
    agent_status_timeout: "超时",
    result_timeout: " · timeout",
    edit_merge_summary: "{} 处修改",
    tools_footer_hint_full: "Tab 过滤 · ↑↓ 选择 · PgUp/PgDn 滚动 · Esc 返回",
    tools_footer_hint_compact: "Tab 过滤 · ↑↓ 选择 · Esc 返回",
};

static EN: UiText = UiText {
    perm_readonly: "request approval",
    perm_full: "full access (no prompts)",
    perm_workspace: "assisted",
    send: "send",
    newline: "newline",
    composer_placeholder: "Type a message, / for commands",
    cancel: "cancel",
    quit: "quit",
    jump_bottom: "bottom",
    waiting_model: "waiting for model",
    goal_mode: "goal mode",
    ctrl_c_cancel: "Ctrl+C cancel",
    waiting_reply: "◇ waiting for your reply",
    back_to_bottom: "Back at bottom",
    context_label: "context",
    context_tokens_only: "context {} tokens",
    context_empty_window: "context window {} tokens · usage after first turn",
    context_with_bar: "context {} {} / {}",
    cached_pct: " ({}% cached)",
    suggest_compact: "try /compact",
    compact_hint: "/compact",
    turn_completed: "✓ task complete",
    turn_answered: "✓ answer finished",
    turn_truncated: "⚠ output truncated · continue if needed",
    turn_incomplete: "⚠ task incomplete",
    turn_unverified: "✓ done · not auto-verified",
    turn_no_automatic_verification: "no auto-verify configured",
    turn_no_code_changes: "◇ ended · repo unchanged",
    turn_failed: "✗ stopped · see error above",
    turn_cancelled: "■ stopped · cancelled",
    turn_budget_exhausted: "budget exhausted · say continue or /goal",
    turn_stalled_goal: "goal not confirmed complete",
    turn_observe_thrash: "no progress · observe thrash stopped",
    turn_plan_thrash: "plan complete · observe thrash stopped",
    tool_calls_n: " · {} tools",
    recap_label: "recap",
    recap_next_step: "Next step: ",
    sub_agent_default: "Sub-agent",
    sub_agent_explorer: "Explorer agent",
    sub_agent_worker: "Worker agent",
    sub_agent_waiting: "waiting",
    sub_agent_running: "running",
    sub_agent_completed: "completed",
    sub_agent_incomplete: "incomplete",
    sub_agent_round_limit: "Did not finish within {} rounds.",
    sub_agent_latest_note: "Latest progress: ",
    sub_agent_task: "Task: ",
    sub_agent_result: "Result: ",
    sub_agent_cached: "cached",
    agents_sub_agents: "Sub-agents",
    agents_orchestrator: "Orchestrator",
    agents_idle: "idle",
    agents_empty: "No agent activity yet (spawn_agent delegates work; /workflow creates workflow agents)",
    agents_scroll_hint: "↑↓/PgUp/PgDn scroll · Esc back",
    unsupported_task_action: "Delegation (unsupported)",
    unsupported_task_hint: "task is unsupported; use spawn_agent",
    plan_update_rejected: "plan unchanged: complete steps in order",
    observe_denied: "skipped: repeated check not needed",
    tool_group_calls: "{} tool calls",
    tool_group_running: "running {}",
    tool_group_succeeded: "{} succeeded",
    tool_group_adjustment: "{} need adjustment",
    tool_group_expand: "expand",
    tool_group_collapse: "collapse",
    tool_details: "Details",
    tool_status_running: "running",
    tool_status_succeeded: "succeeded",
    tool_status_failed: "failed",
    tool_output_lines: "{} lines",
    activity_explored: "Inspected project structure",
    activity_reads: "read {}",
    activity_searches: "search {}",
    activity_symbols: "symbols {}",
    activity_found_locations: "Found {} related code locations",
    file_mention: "file",
    welcome_back: "Welcome back, {}",
    splash_tagline: "AI Coding Agent · different models · one standard",
    splash_hint: "type a task · / commands · ↑ history",
    splash_tips_title: "Getting started",
    splash_tips_lead: "Type a task to start, or use a command:",
    branch_label: "branch ",
    help_commands: "Commands",
    help_keys: "Keys",
    help_view_all: " for all commands",
    help_scroll: "↑↓/PgUp/PgDn scroll · Esc back",
    help_title: "Help",
    key_submit: "submit",
    key_newline: "newline",
    key_cancel_quit: "cancel / quit",
    key_screens: "steps/diff/verify/tools/sessions/agents",
    key_expand: "Ctrl+O: expand/collapse live thinking, else only the latest tool group",
    key_turn_nav: "jump user turns",
    key_model: "switch model",
    key_jump: "jump to bottom after scrolling history",
    key_end: "empty: jump bottom · with text: end of line",
    key_tab: "complete command",
    key_esc: "close screen / overlay",
    turn_nav: "turn {}/{}",
    turn_nav_live: "back to live",
    turn_nav_empty: "no user messages yet",
    screen_context: "Context",
    screen_sessions: "Sessions",
    screen_tools: "Tools",
    screen_plan: "Task steps",
    screen_diff: "Diff",
    screen_verify: "Verification",
    screen_agents: "Agents",
    no_sessions: "No sessions",
    no_context: "No context yet (appears after /workflow runs)",
    no_plan: "No task steps yet (enable /workflow, then run a task)",
    plan_step_of: "step",
    no_diff: "No changes",
    no_verify: "No verification results",
    active_plan: "plan",
    active_rules: "project rules",
    estimated_tokens: "Estimated tokens: {}",
    candidate_files: "Candidate files ({})",
    thinking: "thinking",
    thinking_lines: " · {} lines",
    expand_thinking_tools: "  … Ctrl+O expand live thinking (else latest tool group)",
    queue_retry: "↻ retry",
    queue_sending: "→ sending",
    queue_waiting: "⏳ queued",
    queue_backspace: "  (backspace removes last)",
    queue_count: "⏳ queue {}",
    queue_retry_n: " ({} retry)",
    queue_next: "next: ",
    queue_del_hint: "⌫ pop last",
    queue_actions_hint: "  Enter run now · Delete cancel · Alt+↑↓ reorder",
    queue_starting_now: "interrupting current turn to run this now",
    slash: SlashText {
        model: "switch AI model",
        mode: "permission (request-approval / assisted / full-access)",
        goal: "run a goal task",
        btw: "side question (not in main history)",
        workflow: "toggle direct / workflow",
        work_mode: "work profile economy|balanced|delivery",
        collab: "collaboration chat|plan|goal",
        plan_collab: "collaboration=plan (read-only)",
        confirm_plan: "confirm plan and auto-enter goal",
        memory: "project memory /memory · /memory forget <id>",
        skill: "use a skill (injects $name like typing $skill)",
        steps: "view task steps (after workflow runs)",
        diff: "view diff",
        verify: "view verification",
        tools: "view tool calls",
        sessions: "session list",
        context: "context usage",
        agents: "multi-agent status",
        restore: "restore checkpoint",
        compact: "compact context",
        export: "export conversation to markdown (cwd by default; /export <path>)",
        web: "open the browser Web UI (loopback + token, same session)",
        image: "attach image",
        attach: "add attachment",
        paste: "paste clipboard image",
        theme: "choose theme (ion / night / day)",
        clear: "new chat (clear context)",
        help: "show help",
        quit: "quit",
    },
    slash_ghost: SlashGhost {
        btw: "<question>",
        btw_spaced: " <question>",
        goal: "<goal>",
        goal_spaced: " <goal>",
        path: "<path>",
        path_spaced: " <path>",
        skill: "<skill-name> [task]",
        skill_spaced: " <skill-name> [task]",
    },
    queued_n: "queued {} message(s) for after this turn",
    cleared_queue: "queue cleared",
    deleted_queue_n: "removed one queued item, {} left",
    theme_dark: "switched to dark theme",
    theme_light: "switched to light theme",
    theme_ion_label: "Ion (default)",
    theme_ion_desc: "cool cyan brand palette for dark terminals",
    theme_night_label: "Night",
    theme_night_desc: "deeper blue-violet night palette",
    theme_day_label: "Day",
    theme_day_desc: "light palette for bright terminals",
    mode_workflow_on: "workflow mode (understand → plan → execute → verify)",
    mode_workflow_off: "direct mode",
    cancelled_continue: "cancelled — you can keep typing",
    mode_plan_desc: "always ask: external writes, network, and dangerous commands",
    mode_write_desc: "assisted: reads/writes, network, and shell (incl. git push) auto-run; only delete/privilege/host-open need approval",
    mode_full_desc: "no prompts: edits, dangerous commands, deletes, and memory writes all auto-run",
    overlay_approval: "waiting for approval",
    overlay_clarify: "agent needs clarification",
    overlay_model: "select model",
    overlay_mode: "select permission mode",
    overlay_theme: "select theme",
    overlay_media: "model has no vision",
    overlay_checkpoint: "restore checkpoint",
    pending_n: "+{} waiting",
    btw_label: "btw",
    btw_q: "Q",
    btw_a: "A",
    btw_usage: "usage: /btw <question>",
    btw_failed: "btw failed",
    btw_answering: "Answering…",
    btw_dismiss: "dismiss",
    final_completed: "Completed",
    final_completed_warnings: "Completed with warnings",
    final_waiting_confirmation: "Waiting for confirmation",
    final_blocked: "Incomplete",
    final_verification_failed: "Verification failed",
    final_failed: "Failed",
    final_cancelled: "Cancelled",
    turn_end_completed: "Task completed",
    completion_files_changed: "{} files changed",
    completion_verified: "verification {}/{} passed",
    completion_diff_hint: "/diff to view changes",
    fold_more_lines: "… {} more lines · Ctrl+O to expand",
    fold_more_lines_short: "(+{} lines · Ctrl+O)",
    fold_full_diff: "… Ctrl+O for full diff",
    tool_label_args: "Arguments",
    tool_label_patch: "Patch",
    tool_label_replace: "Text replacement",
    tool_word_blocked: "Blocked",
    tool_word_done: "Done",
    tools_none: "No tool calls",
    tools_col_tool: "Tool",
    tools_col_status: "Status",
    tools_status_running: "Running",
    tools_status_ok: "OK",
    tools_status_attention: "Needs attention",
    tools_col_duration: "Duration",
    tools_output: "Output",
    parallel_header: "{} tools in parallel",
    agents_running_header: "{} agents running",
    agents_done_header: "{} agents completed",
    agents_ended_header: "{} agents finished",
    agent_status_running: "running",
    agent_status_completed: "completed",
    agent_status_timeout: "timeout",
    result_timeout: " · timeout",
    edit_merge_summary: "{} changes",
    tools_footer_hint_full: "Tab filter · ↑↓ select · PgUp/PgDn scroll · Esc back",
    tools_footer_hint_compact: "Tab filter · ↑↓ select · Esc back",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_en_variants() {
        assert_eq!(Locale::parse_override("en"), Locale::En);
        assert_eq!(Locale::parse_override("en_US"), Locale::En);
        assert_eq!(Locale::parse_override("EN-us"), Locale::En);
    }

    #[test]
    fn override_non_en_is_zh() {
        assert_eq!(Locale::parse_override("zh"), Locale::Zh);
        assert_eq!(Locale::parse_override("zh_CN"), Locale::Zh);
        assert_eq!(Locale::parse_override("ja"), Locale::Zh);
        assert_eq!(Locale::parse_override(""), Locale::Zh);
    }

    #[test]
    fn locale_tags() {
        assert_eq!(Locale::from_locale_tag("en_US.UTF-8"), Some(Locale::En));
        assert_eq!(Locale::from_locale_tag("zh_CN.UTF-8"), Some(Locale::Zh));
        assert_eq!(Locale::from_locale_tag("zh-Hans"), Some(Locale::Zh));
        assert_eq!(Locale::from_locale_tag("C"), None);
        assert_eq!(Locale::from_locale_tag("POSIX"), None);
        assert_eq!(Locale::from_locale_tag("ja_JP.UTF-8"), None);
    }

    #[test]
    fn tables_differ_on_a_marker_string() {
        assert_ne!(
            Locale::Zh.text().waiting_model,
            Locale::En.text().waiting_model
        );
        assert_eq!(Locale::Zh.text().perm_workspace, "替我审批");
        assert_eq!(Locale::En.text().perm_workspace, "assisted");
        assert!(Locale::Zh.text().perm_full.contains("免审批"));
        assert!(Locale::Zh.text().mode_full_desc.contains("免审批"));
    }
}
