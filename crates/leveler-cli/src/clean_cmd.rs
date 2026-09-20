//! `leveler clean` — analyze and reclaim CodeLeveler's own local storage.
//!
//! Default is a **dry run**: it reports what could be reclaimed and deletes
//! nothing. Action flags (`--safe`, `--cache`, `--ephemeral`, `--stale-state`)
//! execute a frozen plan, and `--needs-confirmation` is the only way to touch
//! items that might hold durable data. Durable state (sessions, artifacts) is
//! never removed by `--safe`.

use std::path::PathBuf;

use leveler_project::Layout;
use leveler_project::hygiene::{
    CleanupKind, CleanupOptions, CleanupPlan, CleanupReport, DEFAULT_CACHE_IDLE_TTL,
    DEFAULT_EPHEMERAL_TTL, DEFAULT_TOOL_CACHE_BUDGET_BYTES, Safety, ephemeral_base_dir,
    execute_plan_under, plan_cleanup,
};
use leveler_tui::Locale;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Zh,
    En,
}

impl Lang {
    fn detect() -> Self {
        match Locale::resolve(None) {
            Locale::En => Self::En,
            Locale::Zh => Self::Zh,
        }
    }

    fn pick(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }
}

pub struct CleanArgs {
    pub safe: bool,
    pub cache: bool,
    pub ephemeral: bool,
    pub stale_state: bool,
    pub needs_confirmation: bool,
    pub json: bool,
    pub manifest: Option<PathBuf>,
}

/// Options assembled from the user's `[storage]` config, falling back to the
/// built-in policy. Centralised so the CLI and a future TUI entry agree.
fn options_from_config() -> CleanupOptions {
    let mut options = CleanupOptions::default();
    let Ok(config) = leveler_app::GlobalConfig::load() else {
        return options;
    };
    if let Some(bytes) = config.tool_cache_max_bytes() {
        options.tool_cache_budget_bytes = bytes;
    }
    if let Some(hours) = config.ephemeral_ttl_hours() {
        options.ephemeral_ttl = std::time::Duration::from_secs(hours.saturating_mul(3600));
    }
    let _ = (
        DEFAULT_TOOL_CACHE_BUDGET_BYTES,
        DEFAULT_EPHEMERAL_TTL,
        DEFAULT_CACHE_IDLE_TTL,
    );
    options
}

fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB * KIB {
        format!("{:.1} GB", value / (KIB * KIB * KIB))
    } else if value >= KIB * KIB {
        format!("{:.1} MB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.1} KB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Display width, counting CJK/fullwidth characters as two columns, so a
/// bilingual column aligns its numbers instead of drifting by label language.
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| {
            let cp = c as u32;
            let wide = (0x1100..=0x115F).contains(&cp)
                || (0x2E80..=0xA4CF).contains(&cp)
                || (0xAC00..=0xD7A3).contains(&cp)
                || (0xF900..=0xFAFF).contains(&cp)
                || (0xFE30..=0xFE4F).contains(&cp)
                || (0xFF00..=0xFF60).contains(&cp)
                || (0xFFE0..=0xFFE6).contains(&cp);
            if wide { 2 } else { 1 }
        })
        .sum()
}

fn pad_label(label: &str, width: usize) -> String {
    let used = display_width(label);
    let mut out = String::from(label);
    for _ in used..width {
        out.push(' ');
    }
    out
}

fn kind_label(kind: CleanupKind, lang: Lang) -> &'static str {
    match kind {
        CleanupKind::ToolCache => lang.pick("工具缓存", "Tool cache"),
        CleanupKind::ExpiredEphemeral => lang.pick("过期临时运行", "Expired temporary"),
        CleanupKind::DeadSocket => lang.pick("无主 socket", "Unowned socket"),
        CleanupKind::OrphanLock => lang.pick("无主 lock", "Unowned lock"),
        CleanupKind::HistoricalAutomation => lang.pick("历史自动化状态", "Historical automation"),
        CleanupKind::DeletedProjectData => lang.pick("已删除项目数据", "Deleted-project data"),
    }
}

fn render_plan(plan: &CleanupPlan, lang: Lang) -> String {
    let mut out = String::new();
    out.push_str(lang.pick("可释放空间", "Reclaimable storage"));
    out.push_str("\n\n");
    let mut any = false;
    for kind in CleanupKind::ALL {
        let bytes = plan.bytes_for(kind);
        let count = plan.count_for(kind);
        if bytes == 0 && count == 0 {
            continue;
        }
        any = true;
        let safety = if plan
            .entries
            .iter()
            .any(|e| e.kind == kind && e.safety == Safety::NeedsConfirmation)
        {
            lang.pick("  (需要确认)", "  (needs confirmation)")
        } else {
            ""
        };
        out.push_str(&format!(
            "  {}{:>10}{safety}\n",
            pad_label(kind_label(kind, lang), 24),
            human_bytes(bytes)
        ));
    }
    if !any {
        out.push_str(lang.pick("  （无）\n", "  (nothing)\n"));
    }
    out.push('\n');
    out.push_str(&format!(
        "  {}{:>10}\n",
        pad_label(lang.pick("安全可清理总计", "Safe to reclaim"), 24),
        human_bytes(plan.safe_bytes())
    ));
    if plan.needs_confirmation_bytes() > 0 {
        out.push_str(&format!(
            "  {}{:>10}\n",
            pad_label(lang.pick("需要确认", "Needs confirmation"), 24),
            human_bytes(plan.needs_confirmation_bytes())
        ));
    }
    out.push('\n');
    out.push_str(lang.pick(
        "默认不删除任何内容。运行 `leveler clean --safe` 执行安全清理。",
        "Nothing was deleted. Run `leveler clean --safe` to reclaim safe items.",
    ));
    out.push('\n');
    out
}

fn render_report(report: &CleanupReport, lang: Lang) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "✓ {}{}\n\n",
        lang.pick("已释放 ", "Reclaimed "),
        human_bytes(report.reclaimed_bytes)
    ));
    for (kind, bytes) in &report.reclaimed_by_kind {
        if *bytes == 0 {
            continue;
        }
        out.push_str(&format!(
            "  {}{:>10}\n",
            pad_label(kind_label(*kind, lang), 24),
            human_bytes(*bytes)
        ));
    }
    out.push_str(&format!(
        "  {}{:>10}\n",
        pad_label(lang.pick("条目", "Items"), 24),
        report.removed
    ));
    if !report.failures.is_empty() {
        out.push('\n');
        out.push_str(lang.pick("部分数据未能清理\n", "Some items could not be cleaned\n"));
        out.push_str(&format!(
            "  {}{:>10}\n",
            pad_label(lang.pick("未处理", "Remaining"), 24),
            format!(
                "{} · {} 项",
                human_bytes(report.failed_bytes()),
                report.failures.len()
            )
        ));
        for failure in report.failures.iter().take(5) {
            out.push_str(&format!(
                "    {} — {}\n",
                failure.path.display(),
                failure.error
            ));
        }
    }
    out
}

pub fn cmd_clean(layout: Layout, args: CleanArgs) -> anyhow::Result<std::process::ExitCode> {
    let lang = Lang::detect();
    let options = options_from_config();
    let ephemeral = ephemeral_base_dir();
    let plan = plan_cleanup(layout.home(), &ephemeral, &options);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        if !args.safe && !args.cache && !args.ephemeral && !args.stale_state {
            return Ok(std::process::ExitCode::SUCCESS);
        }
    }

    let acting =
        args.safe || args.cache || args.ephemeral || args.stale_state || args.needs_confirmation;
    if !acting {
        print!("{}", render_plan(&plan, lang));
        return Ok(std::process::ExitCode::SUCCESS);
    }

    // Freeze the selection from the plan: no re-scan between here and deletion.
    let mut selection = plan.clone();
    if args.safe {
        selection.safe_only();
    } else if args.cache || args.ephemeral || args.stale_state {
        let mut kinds = Vec::new();
        if args.cache {
            kinds.push(CleanupKind::ToolCache);
        }
        if args.ephemeral {
            kinds.push(CleanupKind::ExpiredEphemeral);
        }
        if args.stale_state {
            kinds.push(CleanupKind::DeadSocket);
            kinds.push(CleanupKind::OrphanLock);
            kinds.push(CleanupKind::HistoricalAutomation);
        }
        selection.retain_kinds(&kinds);
        selection.safe_only();
    } else {
        selection.safe_only();
    }
    if args.needs_confirmation {
        // The only path that may touch durable-adjacent state: explicitly add
        // back the confirmation entries from the frozen plan.
        for entry in plan
            .entries
            .iter()
            .filter(|e| e.safety == Safety::NeedsConfirmation)
        {
            if !selection.entries.iter().any(|e| e.path == entry.path) {
                selection.entries.push(entry.clone());
            }
        }
    }

    if let Some(path) = &args.manifest {
        std::fs::write(path, serde_json::to_string_pretty(&selection)?)?;
        eprintln!(
            "{} {}",
            lang.pick("已写入清理清单：", "cleanup manifest written:"),
            path.display()
        );
    }

    let roots = vec![layout.home().root().to_path_buf(), ephemeral];
    let report = execute_plan_under(&selection, false, &roots);
    print!("{}", render_report(&report, lang));
    Ok(std::process::ExitCode::SUCCESS)
}

/// Translate the hygiene plan into the TUI's plain view model. The TUI never
/// links the storage engine; it only renders what the host hands it.
fn plan_view(plan: &CleanupPlan) -> leveler_tui::clean::CleanPlanView {
    use leveler_tui::clean::{CleanCategory, CleanKind, CleanPlanView, CleanSafety};
    let categories = CleanupKind::ALL
        .iter()
        .filter_map(|kind| {
            let bytes = plan.bytes_for(*kind);
            let count = plan.count_for(*kind);
            if bytes == 0 && count == 0 {
                return None;
            }
            let safety = if plan
                .entries
                .iter()
                .any(|e| e.kind == *kind && e.safety == Safety::NeedsConfirmation)
            {
                CleanSafety::NeedsConfirmation
            } else {
                CleanSafety::Safe
            };
            Some(CleanCategory {
                kind: view_kind(*kind),
                bytes,
                count,
                safety,
            })
        })
        .collect::<Vec<_>>();
    let _ = (CleanKind::ToolCache,);
    CleanPlanView {
        categories,
        safe_bytes: plan.safe_bytes(),
        needs_confirmation_bytes: plan.needs_confirmation_bytes(),
    }
}

fn view_kind(kind: CleanupKind) -> leveler_tui::clean::CleanKind {
    use leveler_tui::clean::CleanKind;
    match kind {
        CleanupKind::ToolCache => CleanKind::ToolCache,
        CleanupKind::ExpiredEphemeral => CleanKind::ExpiredEphemeral,
        CleanupKind::DeadSocket => CleanKind::DeadSocket,
        CleanupKind::OrphanLock => CleanKind::OrphanLock,
        CleanupKind::HistoricalAutomation => CleanKind::HistoricalAutomation,
        CleanupKind::DeletedProjectData => CleanKind::DeletedProjectData,
    }
}

fn result_view(report: &CleanupReport) -> leveler_tui::clean::CleanResultView {
    use leveler_tui::clean::{CleanFailure, CleanResultView};
    CleanResultView {
        reclaimed_bytes: report.reclaimed_bytes,
        removed: report.removed,
        by_kind: report
            .reclaimed_by_kind
            .iter()
            .map(|(kind, bytes)| (view_kind(*kind), *bytes))
            .collect(),
        failures: report
            .failures
            .iter()
            .map(|failure| CleanFailure {
                path: failure.path.display().to_string(),
                kind: leveler_tui::clean::CleanKind::ToolCache,
                reason: failure.error.clone(),
            })
            .collect(),
    }
}

/// The TUI `/clean` host: the same scan/execute the CLI command uses, exposed
/// as two blocking closures. The scan freezes the plan the run will execute, so
/// the page acts on exactly what it showed.
pub fn clean_host() -> leveler_tui::CleanHost {
    use std::sync::{Arc, Mutex};
    let frozen: Arc<Mutex<Option<CleanupPlan>>> = Arc::new(Mutex::new(None));
    let scan_frozen = frozen.clone();
    let scan = Arc::new(move || {
        match std::panic::catch_unwind(|| {
            let options = options_from_config();
            let home = leveler_core::LevelerHome::resolve(leveler_core::environment());
            plan_cleanup(&home, &ephemeral_base_dir(), &options)
        }) {
            Ok(plan) => {
                let view = plan_view(&plan);
                *scan_frozen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(plan);
                view
            }
            Err(_) => leveler_tui::clean::CleanPlanView::default(),
        }
    });
    let run_safe = Arc::new(move || {
        let plan = frozen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_default();
        let mut selection = plan;
        selection.safe_only();
        let home = leveler_core::LevelerHome::resolve(leveler_core::environment());
        let roots = vec![home.root().to_path_buf(), ephemeral_base_dir()];
        let report = execute_plan_under(&selection, false, &roots);
        result_view(&report)
    });
    leveler_tui::CleanHost { scan, run_safe }
}
