//! Task contract parsed from the user goal (user-turn injection, not system).

use serde::{Deserialize, Serialize};

/// Structured view of a task description (Appendix A).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskContract {
    /// Raw user text.
    pub raw: String,
    pub context: Option<String>,
    pub request: Option<String>,
    pub output_format: Option<String>,
    pub constraints: Option<String>,
    pub pause: Option<String>,
    /// Explicit acceptance commands (optional hard checks).
    pub acceptance_commands: Vec<String>,
}

impl TaskContract {
    /// Parse free-form task text into sections. Known limit: code fences may
    /// still match section headers (Appendix A).
    pub fn parse(raw: &str) -> Self {
        let mut c = TaskContract {
            raw: raw.to_string(),
            ..Default::default()
        };
        if raw.trim().is_empty() {
            return c;
        }

        if !raw.lines().any(|line| match_header(line.trim()).is_some()) {
            c.request = Some(raw.trim().to_string());
            return c;
        }

        let mut current: Option<&'static str> = None;
        let mut buf = String::new();

        for line in raw.lines() {
            let trimmed = line.trim();
            if let Some((key, inline)) = match_header(trimmed) {
                flush_section(&mut c, current, &mut buf);
                current = Some(key);
                if let Some(rest) = inline
                    && !rest.is_empty()
                {
                    buf.push_str(rest);
                    buf.push('\n');
                }
            } else if current.is_some() {
                buf.push_str(line);
                buf.push('\n');
            }
        }
        flush_section(&mut c, current, &mut buf);

        if let Some(constraints) = &c.constraints {
            for line in constraints.lines() {
                let t = line.trim();
                if let Some(rest) = t
                    .strip_prefix("accept:")
                    .or_else(|| t.strip_prefix("Accept:"))
                    .or_else(|| t.strip_prefix("acceptance:"))
                {
                    let cmd = rest.trim();
                    if !cmd.is_empty() {
                        c.acceptance_commands.push(cmd.to_string());
                    }
                }
            }
        }
        c
    }

    /// User-turn injection block (must NOT go into cache-stable system prefix).
    pub fn user_injection(&self) -> String {
        let mut out = String::from("## Task contract\n");
        if let Some(r) = &self.request {
            out.push_str(&format!("Request: {r}\n"));
        }
        if let Some(ctx) = &self.context {
            out.push_str(&format!("Context: {ctx}\n"));
        }
        if let Some(o) = &self.output_format {
            out.push_str(&format!("Output format: {o}\n"));
        }
        if let Some(cons) = &self.constraints {
            out.push_str(&format!("Constraints: {cons}\n"));
        }
        if let Some(p) = &self.pause {
            out.push_str(&format!("Pause: {p}\n"));
        }
        if !self.acceptance_commands.is_empty() {
            out.push_str("Acceptance commands:\n");
            for cmd in &self.acceptance_commands {
                out.push_str(&format!("- {cmd}\n"));
            }
        }
        out
    }
}

fn match_header(line: &str) -> Option<(&'static str, Option<&str>)> {
    let lower = line.to_ascii_lowercase();
    for (header, key) in [
        ("context", "context"),
        ("request", "request"),
        ("output format", "output"),
        ("output", "output"),
        ("constraints", "constraints"),
        ("pause", "pause"),
        ("acceptance", "acceptance"),
    ] {
        if lower == header {
            return Some((key, None));
        }
        let prefix = format!("{header}:");
        if lower.starts_with(&prefix) {
            let original_rest = line.get(prefix.len()..).unwrap_or("").trim();
            return Some((key, Some(original_rest)));
        }
    }
    None
}

fn flush_section(c: &mut TaskContract, current: Option<&str>, buf: &mut String) {
    let Some(key) = current else {
        buf.clear();
        return;
    };
    let t = buf.trim().to_string();
    buf.clear();
    if t.is_empty() && key != "acceptance" {
        return;
    }
    match key {
        "context" => c.context = Some(t),
        "request" => c.request = Some(t),
        "output" => c.output_format = Some(t),
        "constraints" => c.constraints = Some(t),
        "pause" => c.pause = Some(t),
        "acceptance" => {
            for line in t.lines() {
                let cmd = line.trim().trim_start_matches('-').trim();
                if !cmd.is_empty() {
                    c.acceptance_commands.push(cmd.to_string());
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_paragraph_is_request() {
        let c = TaskContract::parse("fix the login bug");
        assert_eq!(c.request.as_deref(), Some("fix the login bug"));
        assert!(c.context.is_none());
    }

    #[test]
    fn labeled_sections() {
        let c =
            TaskContract::parse("Request:\n修 bug\nConstraints:\n不改 API\naccept: cargo test\n");
        assert!(
            c.request.as_deref().unwrap().contains("修 bug"),
            "{:?}",
            c.request
        );
        assert!(
            c.constraints.as_deref().unwrap().contains("不改 API"),
            "{:?}",
            c.constraints
        );
        assert_eq!(c.acceptance_commands, vec!["cargo test".to_string()]);
    }

    #[test]
    fn request_header_case_insensitive() {
        let c = TaskContract::parse("REQUEST:\ndo the thing\n");
        assert_eq!(c.request.as_deref(), Some("do the thing"));
    }

    #[test]
    fn empty_string() {
        let c = TaskContract::parse("");
        assert!(c.raw.is_empty());
        assert!(c.request.is_none());
    }

    #[test]
    fn user_injection_keeps_raw_sections() {
        let c = TaskContract::parse("Request:\nship it\n");
        let inj = c.user_injection();
        assert!(inj.contains("Task contract"));
        assert!(inj.contains("ship it"));
    }
}
