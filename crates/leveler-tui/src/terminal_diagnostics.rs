//! Local terminal observations; environment hints never prove clipboard delivery.
use std::io::IsTerminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Unknown,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalDiagnostics {
    pub remote: bool,
    pub tmux: bool,
    pub stdout_terminal: bool,
    pub osc52: Capability,
    pub term: Option<String>,
    pub colorterm: Option<String>,
}

impl TerminalDiagnostics {
    pub fn detect() -> Self {
        Self::from_environment(
            |key| std::env::var(key).ok(),
            std::io::stdout().is_terminal(),
        )
    }
    pub fn from_environment(get: impl Fn(&str) -> Option<String>, stdout_terminal: bool) -> Self {
        let present = |key| get(key).is_some_and(|value| !value.is_empty());
        let term = get("TERM");
        let remote = present("SSH_CONNECTION") || present("SSH_CLIENT") || present("SSH_TTY");
        let tmux = present("TMUX");
        Self {
            remote,
            tmux,
            stdout_terminal,
            osc52: if !stdout_terminal
                || term.as_deref() == Some("dumb")
                || (!remote && !tmux && get("TERM_PROGRAM").as_deref() == Some("Apple_Terminal"))
            {
                Capability::Unavailable
            } else {
                Capability::Unknown
            },
            term,
            colorterm: get("COLORTERM"),
        }
    }
    /// Does not probe or modify the clipboard, terminal, or tmux configuration.
    pub fn describe(&self) -> String {
        self.describe_localized(crate::i18n::Locale::En)
    }
    pub fn describe_localized(&self, locale: crate::i18n::Locale) -> String {
        let clean = |value: Option<&str>| -> String {
            value
                .unwrap_or("unknown")
                .chars()
                .filter(|c| !c.is_control())
                .take(128)
                .collect()
        };
        let term = clean(self.term.as_deref());
        let colorterm = clean(self.colorterm.as_deref());
        let no_color = crate::theme::Theme::env_no_color();
        if locale == crate::i18n::Locale::Zh {
            return format!(
                "TERM（环境声明）: {term}\nCOLORTERM（环境声明）: {colorterm}\nNO_COLOR（沿用主题设置）: {no_color}\n终端实际颜色能力: 未知，未探测\nstdout 是终端: {}\n检测到 SSH: {}\n检测到 tmux: {}\n原生剪贴板: {}\nOSC 52: {}（送达未经验证）\ntmux buffer: {}\ntmux 剪贴板转发 / DCS passthrough: 未知\nOSC 52 原文上限: {} 字节\n换行绑定: Ctrl+J / Alt+Enter / Ctrl+Enter\nShift+Enter: 未绑定；终端扩展按键传输能力未知\n本次诊断没有读写剪贴板，也没有修改终端或 tmux 配置。",
                self.stdout_terminal,
                self.remote,
                self.tmux,
                if self.remote {
                    "远程主机跳过"
                } else if self.tmux {
                    "跳过：tmux 当前客户端位置未知"
                } else {
                    "未探测"
                },
                match self.osc52 {
                    Capability::Unknown => "未知",
                    Capability::Unavailable => "不可用",
                },
                if self.tmux {
                    "检测到路由；未探测"
                } else {
                    "未检测到"
                },
                crate::clipboard::MAX_OSC52_BYTES
            );
        }
        format!(
            "TERM (environment reported): {term}\nCOLORTERM (environment reported): {colorterm}\nNO_COLOR (theme setting): {no_color}\nObserved terminal color capability: unknown; not probed\nstdout TTY: {}\nSSH detected: {}\ntmux detected: {}\nNative clipboard: {}\nOSC 52: {} (delivery is not verified)\ntmux buffer: {}\ntmux clipboard forwarding / DCS passthrough: unknown\nOSC 52 payload limit: {} bytes\nNewline bindings: Ctrl+J / Alt+Enter / Ctrl+Enter\nShift+Enter: not bound; terminal extended-key delivery is unknown\nNo clipboard read/write or configuration changes were performed.",
            self.stdout_terminal,
            self.remote,
            self.tmux,
            if self.remote {
                "skipped on remote host"
            } else if self.tmux {
                "skipped: tmux client location unknown"
            } else {
                "not probed"
            },
            match self.osc52 {
                Capability::Unknown => "unknown",
                Capability::Unavailable => "unavailable",
            },
            if self.tmux {
                "available route; not probed"
            } else {
                "not detected"
            },
            crate::clipboard::MAX_OSC52_BYTES
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn environment_cannot_prove_clipboard_support() {
        let d = TerminalDiagnostics::from_environment(
            |key| match key {
                "TERM" => Some("xterm-256color".into()),
                "SSH_TTY" => Some("/dev/pts/1".into()),
                "TMUX" => Some("socket,1,0".into()),
                _ => None,
            },
            true,
        );
        assert!(d.remote && d.tmux);
        assert_eq!(d.osc52, Capability::Unknown);
        assert!(d.describe().contains("not verified"));
    }
    #[test]
    fn never_emit_control_sequences_to_piped_output() {
        let d = TerminalDiagnostics::from_environment(|_| None, false);
        assert_eq!(d.osc52, Capability::Unavailable);
    }
    #[test]
    fn inherited_terminal_program_cannot_identify_remote_or_tmux_client() {
        for key in ["SSH_CONNECTION", "TMUX"] {
            let d = TerminalDiagnostics::from_environment(
                |name| {
                    if name == "TERM_PROGRAM" {
                        Some("Apple_Terminal".into())
                    } else if name == key {
                        Some("present".into())
                    } else {
                        None
                    }
                },
                true,
            );
            assert_eq!(d.osc52, Capability::Unknown);
        }
    }
}
