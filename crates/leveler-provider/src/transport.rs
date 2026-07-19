//! HTTP transport: sends encoded requests with retry/backoff and maps every
//! failure onto a normalized [`ModelError`].

use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use leveler_model::{ModelError, ModelErrorKind, ProtocolContext, RawByteStream};

use crate::config::RetryConfig;

/// Map a `reqwest` transport error to a normalized model error.
pub(crate) fn map_reqwest_error(err: &reqwest::Error) -> ModelError {
    let kind = if err.is_timeout() {
        ModelErrorKind::Timeout
    } else if err.is_connect() {
        ModelErrorKind::ProviderUnavailable
    } else if err.is_body() || err.is_decode() {
        ModelErrorKind::Decode
    } else {
        ModelErrorKind::Transport
    };
    ModelError::new(kind, err.to_string())
}

/// Build a POST request with auth and per-protocol headers applied.
fn build_request(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    context: &ProtocolContext,
    per_request_timeout: Option<Duration>,
) -> reqwest::RequestBuilder {
    let mut builder = client.post(url).json(body);
    if let Some(key) = &context.api_key {
        builder = builder.bearer_auth(key);
    }
    for (k, v) in &context.extra_headers {
        builder = builder.header(k, v);
    }
    if let Some(timeout) = per_request_timeout {
        builder = builder.timeout(timeout);
    }
    builder
}

/// Send a request, retrying on retryable failures with exponential backoff.
/// Returns the successful (2xx) response, or a normalized error.
pub(crate) async fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    context: &ProtocolContext,
    retry: &RetryConfig,
    per_request_timeout: Option<Duration>,
    cancellation: &CancellationToken,
) -> Result<reqwest::Response, ModelError> {
    let max_attempts = retry.max_attempts.max(1);
    let mut backoff = Duration::from_millis(retry.initial_backoff_ms);
    let max_backoff = Duration::from_millis(retry.max_backoff_ms);
    let mut last_error: Option<ModelError> = None;

    for attempt in 1..=max_attempts {
        if cancellation.is_cancelled() {
            return Err(ModelError::cancelled());
        }

        let request = build_request(client, url, body, context, per_request_timeout);
        let send = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ModelError::cancelled()),
            result = request.send() => result,
        };

        match send {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }
                let code = status.as_u16();
                let retry_after = parse_retry_after(response.headers());
                let text = response.text().await.unwrap_or_default();
                let mut err = ModelError::from_status(code, truncate(&text, 500));
                if let Some(ms) = retry_after {
                    err = err.with_retry_after_ms(ms);
                }
                if !err.retryable {
                    return Err(err);
                }
                if attempt == max_attempts {
                    return Err(exhausted(err));
                }
                last_error = Some(err);
            }
            Err(e) => {
                let err = map_reqwest_error(&e);
                if !err.retryable {
                    return Err(err);
                }
                if attempt == max_attempts {
                    return Err(exhausted(err));
                }
                last_error = Some(err);
            }
        }

        // Backoff before the next attempt, cancellable. A provider-advertised
        // Retry-After overrides the exponential schedule — retrying a rate
        // limit sooner than told just burns the next attempt on another 429.
        let wait = last_error
            .as_ref()
            .and_then(|e| e.retry_after_ms)
            .map(|ms| Duration::from_millis(ms).min(MAX_RETRY_AFTER))
            .unwrap_or(backoff);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ModelError::cancelled()),
            _ = tokio::time::sleep(wait) => {}
        }
        backoff = (backoff * 2).min(max_backoff);
    }

    Err(last_error
        .unwrap_or_else(|| ModelError::new(ModelErrorKind::Other, "request failed with no error")))
}

/// Hard cap on a provider-advertised wait so a hostile/buggy `Retry-After`
/// cannot park the turn for minutes.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Parse `Retry-After` as delay-seconds into milliseconds. The HTTP-date form
/// is rare on LLM gateways and is ignored (falls back to exponential backoff).
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|secs| secs.saturating_mul(1000))
}

/// Provider-level retries are the owner of request-start failures. Once they
/// are exhausted, mark the error terminal so an outer agent retry loop cannot
/// multiply N provider attempts into N×M identical waits. Mid-stream failures
/// bypass this function and remain retryable by the agent.
fn exhausted(mut error: ModelError) -> ModelError {
    error.retryable = false;
    error
}

/// Convert a successful streaming response into a raw byte stream with
/// normalized errors.
pub(crate) fn response_to_byte_stream(response: reqwest::Response) -> RawByteStream {
    let stream = response
        .bytes_stream()
        .map(|item| item.map_err(|e| map_reqwest_error(&e)));
    Box::pin(stream)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "áéíóú".repeat(200);
        let t = truncate(&s, 10);
        assert!(t.len() <= 13); // 10 bytes + ellipsis
    }
}
