//! Measurement shim (C5-S2): feed messages as JSON lines on stdin, print the
//! REAL estimator's count. Exists so calibration runs the production
//! estimator, not a reimplementation that could drift.
//!
//! stdin:  one JSON object per line: {"role":"user","text":"..."} for a text
//! part, or {"role":"user","tool_result":"..."} to exercise the tool-payload
//! weighting path.
//! stdout: a single integer — estimate_tokens over all lines as one transcript.

use leveler_agent::estimate_tokens;
use leveler_core::ToolCallId;
use leveler_model::{ContentPart, Message, Role, ToolResultContent};

fn main() {
    let mut messages = Vec::new();
    for line in std::io::stdin().lines() {
        let line = line.expect("stdin");
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(&line).expect("json line");
        let role = match v["role"].as_str().unwrap_or("user") {
            "assistant" => Role::Assistant,
            "system" => Role::System,
            _ => Role::User,
        };
        if let Some(tool) = v["tool_result"].as_str() {
            messages.push(Message {
                role,
                content: vec![ContentPart::ToolResult {
                    result: ToolResultContent {
                        call_id: ToolCallId::new("calibration"),
                        content: tool.to_string(),
                        is_error: false,
                    },
                }],
            });
        } else {
            messages.push(Message::text(
                role,
                v["text"].as_str().unwrap_or("").to_string(),
            ));
        }
    }
    println!("{}", estimate_tokens(&messages));
}
