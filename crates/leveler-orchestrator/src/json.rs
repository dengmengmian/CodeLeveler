//! Structured model output: prompt the model for JSON and parse it robustly.
//!
//! Weaker models wrap JSON in prose or ```json fences, or emit reasoning first.
//! [`extract_json`] pulls the first balanced JSON value out of the text, and
//! [`request_json`] retries once with a stricter instruction on failure.

use tokio_util::sync::CancellationToken;

use leveler_model::{Message, ModelRef, ModelRequest, ModelRuntime, Role};

use crate::error::OrchestratorError;

/// Ask the model to produce JSON deserializable into `T`.
pub async fn request_json<T: serde::de::DeserializeOwned>(
    runtime: &dyn ModelRuntime,
    model: &ModelRef,
    system: &str,
    user: &str,
    cancellation: &CancellationToken,
) -> Result<T, OrchestratorError> {
    let attempt = |extra: Option<&str>| {
        let mut content = user.to_string();
        if let Some(note) = extra {
            content.push_str("\n\n");
            content.push_str(note);
        }
        let messages = vec![
            Message::text(Role::System, system),
            Message::text(Role::User, content),
        ];
        let mut request = ModelRequest::new(model.clone(), messages);
        request.max_output_tokens = Some(2048);
        request.temperature = Some(0.0);
        request
    };

    // First try.
    let response = runtime
        .generate(attempt(None), cancellation.child_token())
        .await?;
    if let Some(value) = extract_json(&response.message.text_content())
        && let Ok(parsed) = serde_json::from_value::<T>(value)
    {
        return Ok(parsed);
    }

    // Retry with a stricter instruction.
    let response = runtime
        .generate(
            attempt(Some(
                "Respond with ONLY a single valid JSON value and no other text.",
            )),
            cancellation.child_token(),
        )
        .await?;
    let text = response.message.text_content();
    let value = extract_json(&text).ok_or_else(|| {
        OrchestratorError::Json(format!("no JSON found in response: {text:.200}"))
    })?;
    serde_json::from_value(value).map_err(|e| OrchestratorError::Json(e.to_string()))
}

/// Extract the first balanced JSON object or array from free-form text.
pub fn extract_json(text: &str) -> Option<serde_json::Value> {
    let trimmed = text.trim();

    // Whole thing parses.
    if let Ok(v) = serde_json::from_str(trimmed) {
        return Some(v);
    }

    // Fenced ```json ... ``` (or plain ```).
    if let Some(inner) = fenced_block(trimmed)
        && let Ok(v) = serde_json::from_str(inner.trim())
    {
        return Some(v);
    }

    // First balanced object/array anywhere in the text.
    balanced_slice(trimmed).and_then(|s| serde_json::from_str(s).ok())
}

fn fenced_block(text: &str) -> Option<&str> {
    let start = text.find("```")?;
    let after = &text[start + 3..];
    // Skip an optional language tag on the same line.
    let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &after[body_start..];
    let end = body.find("```")?;
    Some(&body[..end])
}

/// Return the substring spanning the first balanced `{...}` or `[...]`,
/// respecting strings and escapes.
fn balanced_slice(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{' || b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };

    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            x if x == open => depth += 1,
            x if x == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let v = extract_json(r#"{"a": 1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn parses_fenced_json() {
        let text = "Here is the plan:\n```json\n{\"a\": 2}\n```\nDone.";
        assert_eq!(extract_json(text).unwrap()["a"], 2);
    }

    #[test]
    fn parses_embedded_json_after_reasoning() {
        let text = "Let me think... the answer is {\"goal\": \"x\", \"n\": [1,2]} okay.";
        let v = extract_json(text).unwrap();
        assert_eq!(v["goal"], "x");
    }

    #[test]
    fn handles_braces_in_strings() {
        let text = r#"prefix {"code": "fn f() { }"} suffix"#;
        let v = extract_json(text).unwrap();
        assert_eq!(v["code"], "fn f() { }");
    }

    #[test]
    fn returns_none_without_json() {
        assert!(extract_json("no json here").is_none());
    }
}
