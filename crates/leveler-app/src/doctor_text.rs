//! Localized rendering of the runtime's own diagnostic evidence.
//!
//! `/doctor` shares one [`DiagnosticReport`](leveler_client_protocol::DiagnosticReport)
//! between the CLI and UI clients. The report structure, finding ids,
//! statuses, subjects and raw facts (paths, versions, model ids, error text)
//! are the machine contract and never change here. Only the runtime's *own*
//! user-readable sentences are given a Chinese rendering when the requesting
//! client asks for it; the CLI keeps the English contract because it sends no
//! locale.
//!
//! The mapping is deliberately conservative: an unrecognized sentence is
//! returned untouched rather than reworded or guessed at.

use leveler_client_protocol::DiagnosticFinding;

/// Translate the runtime-owned evidence sentences of `findings` to Chinese.
/// Facts such as `write=Supported`, `$ENV` names, paths, model ids and the
/// provider's raw error text stay in their original form.
pub(crate) fn localize_findings(findings: &mut [DiagnosticFinding]) {
    for finding in findings {
        for line in &mut finding.evidence {
            if let Some(translated) = translate(line) {
                *line = translated;
            }
        }
    }
}

fn translate(line: &str) -> Option<String> {
    let exact = match line {
        "not found on PATH (required)" => "PATH 中未找到（必需）",
        "not found on PATH (optional; falls back to built-in)" => {
            "PATH 中未找到（可选；使用内置实现）"
        }
        "not found on PATH (optional)" => "PATH 中未找到（可选）",
        "no sandbox canary ran in this session; backend capability does not prove enforcement" => {
            "本次会话未运行沙箱 canary；后端能力不代表实际执行已受约束"
        }
        "see `leveler agents list`" => "查看 `leveler agents list`",
        "always-on (no effort knob); CodeLeveler default n/a" => {
            "始终开启（无档位）；CodeLeveler 默认值不适用"
        }
        "this model's thinking cannot be adjusted" => "此模型的思考档位不可调整",
        "set via config (api_key)" => "通过配置设置（api_key）",
        "no key required" => "无需密钥",
        "no path (set HOME, USERPROFILE, or LEVELER_HOME)" => {
            "无路径（请设置 HOME、USERPROFILE 或 LEVELER_HOME）"
        }
        "the runtime does not expose a configuration reload generation to clients; the active source and parse result are checked above" => {
            "运行时未向客户端暴露配置重载代数；上方已检查当前配置源与解析结果"
        }
        "model.instructions is retired and ignored; all models now share the CodeLeveler base prompt — remove the key" => {
            "model.instructions 已废弃并被忽略；所有模型现在共享 CodeLeveler 基础提示 — 请移除该键"
        }
        "MCP server configuration could not be read (I/O or parse error)" => {
            "MCP 服务器配置无法读取（I/O 或解析错误）"
        }
        "no MCP servers configured" => "未配置 MCP 服务器",
        "disabled by configuration" => "已由配置禁用",
        "a connection attempt is in progress" => "正在尝试连接",
        "the previous connection is no longer serving its catalog; open /mcp to reconnect" => {
            "先前的连接不再提供工具目录；打开 /mcp 重新连接"
        }
        "last attempt failed" => "上次尝试失败",
        "the runtime cannot observe this server" => "运行时无法观测此服务器",
        _ => return translate_parameterized(line),
    };
    Some(exact.to_string())
}

fn translate_parameterized(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("none loaded — ") {
        return Some(format!("未加载 — {}", setup_hint(rest)));
    }
    if let Some(rest) = line.strip_prefix("environment variable $")
        && let Some(name) = rest.strip_suffix(" is not set")
    {
        return Some(format!("环境变量 ${name} 未设置"));
    }
    if let Some(rest) = line.strip_prefix("set via $") {
        return Some(format!("通过 ${rest} 设置"));
    }
    if let Some(rest) = line.strip_prefix("ok (") {
        return Some(format!("正常（{}）", rest.trim_end_matches(')')));
    }
    if let Some(rest) = line.strip_prefix("not found at ")
        && let Some(path) = rest.strip_suffix(" — using empty defaults")
    {
        return Some(format!("{path} 不存在 — 使用空默认配置"));
    }
    if let Some(rest) = line.strip_prefix("available: ") {
        return Some(format!("可用：{rest}"));
    }
    if let Some(rest) = line.strip_prefix("explicit thinking budget=") {
        return Some(format!("显式 thinking 预算={rest}"));
    }
    if let Some(rest) = line.strip_prefix("supported=[") {
        // `supported=[…] default=… effective=…` — labels are language,
        // values are the provider's wire facts.
        return Some(format!(
            "支持=[{}",
            rest.replace(" default=", " 默认=")
                .replace(" effective=", " 实际=")
        ));
    }
    if let Some(rest) =
        line.strip_prefix("reasoning=true but effective effort is unknown (supported=[")
        && let Some(values) = rest.strip_suffix("])")
    {
        return Some(format!("reasoning=true 但实际强度未知（支持=[{values}]）"));
    }
    if let Some(rest) = line.strip_prefix("thinking = \"")
        && let Some((level, tail)) = rest.split_once('"')
    {
        if let Some(available) = tail.strip_prefix(" is not available on this model (")
            && let Some(available) =
                available.strip_suffix("); it will use the provider's own default instead")
        {
            return Some(format!(
                "thinking = \"{level}\" 在此模型上不可用（{}）；将改用 provider 自身默认值",
                translate_available(available)
            ));
        }
        if tail.starts_with(" has no effect: this model does not reason; set this model's thinking to \"auto\" to use its native behavior") {
            return Some(format!(
                "thinking = \"{level}\" 无效：此模型不进行推理；把该模型的 thinking 设为 \"auto\" 以使用其原生行为"
            ));
        }
    }
    if let Some(rest) = line.strip_prefix("connected · ") {
        // `connected · N tool(s)` plus an optional ` · protocol X`.
        if let Some((tools, protocol)) = rest.split_once(" · protocol ")
            && let Some(count) = tools.strip_suffix(" tool(s)")
        {
            return Some(format!("已连接 · {count} 个工具 · 协议 {protocol}"));
        }
        if let Some(count) = rest.strip_suffix(" tool(s)") {
            return Some(format!("已连接 · {count} 个工具"));
        }
    }
    if let Some(rest) = line.strip_prefix("configured but not connected (")
        && let Some((endpoint, _action)) = rest.split_once("); open /mcp to connect")
    {
        return Some(format!("已配置但未连接（{endpoint}）；打开 /mcp 连接"));
    }
    if let Some(reason) = line.strip_prefix("last attempt failed: ") {
        return Some(format!("上次尝试失败：{reason}"));
    }
    if let Some(rest) = line.strip_prefix("provider `")
        && let Some((provider, tail)) = rest.split_once('`')
        && tail.starts_with(" is not configured")
    {
        return Some(format!(
            "provider `{provider}` 未配置 — 在 ~/.leveler/config.toml 中添加 [providers.{provider}]"
        ));
    }
    if let Some(rest) = line.strip_prefix("env `")
        && let Some((key, tail)) = rest.split_once('`')
        && tail.starts_with(" is not a valid name reference")
    {
        return Some(format!("env `{key}` 不是有效的名称引用 — 改为 {key}={key}"));
    }
    if let Some(rest) = line.strip_prefix('`')
        && let Some((key, tail)) = rest.split_once('`')
        && tail.starts_with(" is not a valid name reference")
    {
        return Some(format!(
            "`{key}` 不是有效的名称引用 — 请把值改成 UPPER_SNAKE 形式的环境变量名"
        ));
    }
    if let Some(rest) =
        line.strip_suffix(" server(s) configured; this process does not own their connections")
        && let Ok(count) = rest.parse::<usize>()
    {
        return Some(format!("已配置 {count} 个服务器；本进程不持有它们的连接"));
    }
    if let Some(rest) = line.strip_suffix(" loaded")
        && let Ok(count) = rest.parse::<usize>()
    {
        return Some(format!("已加载 {count} 个"));
    }
    if let Some(rest) = line.strip_suffix(" not loaded")
        && let Ok(count) = rest.parse::<usize>()
    {
        return Some(format!("{count} 个未加载"));
    }
    if let Some(rest) = line.strip_suffix(" built-in")
        && let Some((project, rest)) = rest.split_once(" project, ")
        && let Some((user, builtin)) = rest.split_once(" user, ")
    {
        return Some(format!(
            "项目 {project} 个、用户 {user} 个、内置 {builtin} 个"
        ));
    }
    None
}

fn translate_available(available: &str) -> String {
    match available.strip_prefix("available: ") {
        Some(list) => format!("可用：{list}"),
        None => available.to_string(),
    }
}

fn setup_hint(hint: &str) -> String {
    match hint {
        "add providers/models in ~/.leveler/config.toml (or $LEVELER_HOME/config.toml); see README" => {
            "在 ~/.leveler/config.toml（或 $LEVELER_HOME/config.toml）中添加 provider 和 model；参见 README".to_string()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_client_protocol::{DiagnosticCategory, DiagnosticFinding, DiagnosticStatus};

    fn finding(evidence: Vec<&str>) -> DiagnosticFinding {
        DiagnosticFinding {
            id: "mcp_connection".into(),
            subject: Some("fs".into()),
            category: DiagnosticCategory::Mcp,
            severity: leveler_client_protocol::DiagnosticSeverity::Info,
            status: DiagnosticStatus::Pass,
            evidence: evidence.into_iter().map(str::to_string).collect(),
            action: None,
            checked_at: "now".into(),
        }
    }

    #[test]
    fn prose_is_translated_and_facts_are_preserved() {
        let mut findings = vec![finding(vec![
            "not found on PATH (required)",
            "environment variable $OPENAI_KEY is not set",
            "configured but not connected (stdio:server); open /mcp to connect",
            "connected · 3 tool(s) · protocol 2025-06-18",
            "ok (/home/u/.leveler/config.toml)",
        ])];
        localize_findings(&mut findings);
        let evidence = &findings[0].evidence;
        assert!(evidence[0].contains("PATH 中未找到"), "{evidence:?}");
        assert!(
            evidence[1].contains("$OPENAI_KEY") && evidence[1].contains("未设置"),
            "{evidence:?}"
        );
        assert!(evidence[2].contains("stdio:server"), "{evidence:?}");
        assert!(evidence[3].contains("3 个工具"), "{evidence:?}");
        assert!(evidence[3].contains("2025-06-18"), "{evidence:?}");
        assert!(
            evidence[4].contains("/home/u/.leveler/config.toml"),
            "{evidence:?}"
        );
        assert!(evidence[4].starts_with("正常"), "{evidence:?}");
        // Identifiers never change.
        assert_eq!(findings[0].id, "mcp_connection");
        assert_eq!(findings[0].subject.as_deref(), Some("fs"));
    }

    #[test]
    fn unknown_evidence_is_left_untouched() {
        let mut findings = vec![finding(vec![
            "some future sentence the mapping has not seen",
        ])];
        localize_findings(&mut findings);
        assert_eq!(
            findings[0].evidence[0],
            "some future sentence the mapping has not seen"
        );
    }

    #[test]
    fn raw_provider_errors_stay_raw() {
        let mut findings = vec![finding(vec![
            "last attempt failed: connection refused on 127.0.0.1:1",
        ])];
        localize_findings(&mut findings);
        assert!(
            findings[0].evidence[0].contains("connection refused on 127.0.0.1:1"),
            "{:?}",
            findings[0].evidence
        );
    }
}
