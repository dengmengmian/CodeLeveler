//! HTTP transport: sends one physical request and maps every
//! failure onto a normalized [`ModelError`].

use std::time::Duration;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use leveler_model::{DeliveryState, ModelError, ModelErrorKind, ProtocolContext, RawByteStream};

/// Map a `reqwest` transport error to a normalized model error, including what
/// the transport can prove about delivery.
///
/// `is_connect()` is checked first and is the only case that proves the
/// request never left this process (DNS/TCP/TLS failed during connection
/// setup, including a connect timeout), so it is the only case that may be
/// auto-retried. Everything later in the chain is ambiguous — the request may
/// already have reached the provider — and is reported as such rather than
/// guessed.
/// Walk a reqwest error's source chain for a structured [`std::io::ErrorKind`].
///
/// The fault is read from the OS error itself, never from a formatted message:
/// reqwest reports a refused socket as `io::ErrorKind::ConnectionRefused` two
/// links down the chain (`client error (Connect)` → `tcp connect error` →
/// `Connection refused`), and that kind is what decides the retry budget.
fn connect_io_kind(err: &reqwest::Error) -> Option<std::io::ErrorKind> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(err);
    while let Some(error) = source {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            return Some(io.kind());
        }
        source = error.source();
    }
    None
}

pub(crate) fn map_reqwest_error(err: &reqwest::Error) -> ModelError {
    let (kind, delivery_state) = if err.is_connect() {
        (ModelErrorKind::ProviderUnavailable, DeliveryState::NotSent)
    } else if err.is_timeout() {
        (ModelErrorKind::Timeout, DeliveryState::SentNoResponse)
    } else if err.is_body() || err.is_decode() {
        // The provider answered; the response could not be read.
        (ModelErrorKind::Decode, DeliveryState::SentNoResponse)
    } else {
        // A mid-request reset or other transport fault: whether the request
        // was delivered cannot be established here.
        (ModelErrorKind::Transport, DeliveryState::Unknown)
    };
    let mut error = ModelError::new(kind, err.to_string()).with_delivery_state(delivery_state);
    if err.is_connect() && connect_io_kind(err) == Some(std::io::ErrorKind::ConnectionRefused) {
        error = error.with_transport_fault(leveler_model::TransportFault::ConnectionRefused);
    }
    error
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
    // When the protocol supplies its own API-key header (Anthropic's `x-api-key`),
    // do NOT also attach `Authorization: Bearer` — sending both is redundant and
    // some gateways reject the pair. The explicit header wins.
    let has_explicit_api_key = context
        .extra_headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("x-api-key"));
    if let Some(key) = &context.api_key
        && !has_explicit_api_key
    {
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

/// One physical attempt. Retry admission and durable attempt facts belong to
/// the caller; transport must never hide another potentially billed request.
pub(crate) async fn send_once(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    context: &ProtocolContext,
    budget: RequestBudget,
    cancellation: &CancellationToken,
) -> Result<reqwest::Response, ModelError> {
    let per_request_timeout = budget.remaining().ok_or_else(|| {
        ModelError::new(
            ModelErrorKind::Timeout,
            "request deadline elapsed before sending",
        )
        .with_delivery_state(DeliveryState::NotSent)
    })?;
    let request = build_request(client, url, body, context, per_request_timeout);
    let response = tokio::select! {
        biased;
        _=cancellation.cancelled()=>return Err(ModelError::cancelled()),
        result=request.send()=>result.map_err(|error|map_reqwest_error(&error))?,
    };
    if response.status().is_success() {
        return Ok(response);
    }
    let code = response.status().as_u16();
    let retry_after = parse_retry_after(response.headers());
    let header_request_id = request_id_from_headers(response.headers());
    let text = tokio::select! {
        biased;
        _=cancellation.cancelled()=>return Err(ModelError::cancelled()),
        result=response.text()=>result.map_err(|error|map_reqwest_error(&error))?,
    };
    let detail = parse_provider_error(&text);
    let reason = detail
        .message
        .clone()
        .unwrap_or_else(|| truncate(&text, 500));
    let mut error = ModelError::from_status_and_code(code, reason, detail.code.as_deref());
    if let Some(code) = detail.code {
        error = error.with_provider_code(code);
    }
    if let Some(id) = header_request_id.or(detail.request_id) {
        error = error.with_request_id(id);
    }
    if let Some(ms) = retry_after {
        error = error.with_retry_after_ms(ms);
    }
    Err(error)
}

/// What a single provider call may spend: the caller's remaining deadline when
/// it set one, otherwise the provider's configured per-request default.
///
/// A caller deadline is measured from its original clock, while the provider
/// default bounds only this physical request.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RequestBudget {
    /// No caller budget: each attempt gets the provider's configured timeout.
    PerRequest(Option<Duration>),
    /// The caller owns the clock; attempts spend what is left of it.
    Deadline(std::time::Instant),
}

impl RequestBudget {
    /// What this attempt may take, or `None` when the deadline has passed.
    fn remaining(&self) -> Option<Option<Duration>> {
        match self {
            Self::PerRequest(timeout) => Some(*timeout),
            Self::Deadline(deadline) => {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                (!left.is_zero()).then_some(Some(left))
            }
        }
    }
}

/// Structured fields a provider's error response carried.
///
/// Extracted once, here at the transport boundary, so no downstream layer —
/// runtime, transcript, or TUI — ever parses a vendor error body.
#[derive(Debug, Default, PartialEq, Eq)]
struct ProviderErrorDetail {
    code: Option<String>,
    message: Option<String>,
    request_id: Option<String>,
}

/// Extract the error envelope most OpenAI-compatible and Anthropic gateways
/// use. Shape-tolerant and best-effort: an unrecognized body leaves every
/// field `None` and the caller keeps the sanitized raw text as the reason.
fn parse_provider_error(body: &str) -> ProviderErrorDetail {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return ProviderErrorDetail::default();
    };
    let error = value.get("error").unwrap_or(&value);
    let field = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    ProviderErrorDetail {
        code: field(error, "code").or_else(|| field(error, "type")),
        message: field(error, "message"),
        request_id: field(error, "request_id")
            .or_else(|| field(&value, "request_id"))
            .or_else(|| field(&value, "id").filter(|s| s.starts_with("req"))),
    }
}

/// Correlation headers gateways expose, in priority order. A correlation id is
/// the single most useful thing a provider report can carry, so the common
/// spellings are all read.
const REQUEST_ID_HEADERS: &[&str] = &[
    "x-request-id",
    "request-id",
    "x-ms-request-id",
    "x-amzn-requestid",
    "cf-ray",
];

fn request_id_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    for name in REQUEST_ID_HEADERS {
        if let Some(value) = headers.get(*name).and_then(|v| v.to_str().ok()) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

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

/// Convert a successful streaming response into a raw byte stream with
/// normalized errors.
pub(crate) fn response_to_byte_stream(response: reqwest::Response) -> RawByteStream {
    let stream = response
        .bytes_stream()
        .map(|item| item.map_err(|e| map_reqwest_error(&e)));
    Box::pin(stream)
}

fn truncate(s: &str, max: usize) -> String {
    leveler_core::truncate_head_bytes(s, max, "…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "áéíóú".repeat(200);
        let t = truncate(&s, 10);
        assert!(t.len() <= 13); // 10 bytes + ellipsis
    }

    /// The real gateway case: HTTP 400 whose body carries a structured
    /// `error.code = invalid_api_key`. The classification step must read the
    /// structured code, not the HTTP status alone and not the message text.
    #[test]
    fn structured_auth_code_in_a_400_body_classifies_as_auth() {
        let body = r#"{"error":{"code":"invalid_api_key","message":"Incorrect API key provided"}}"#;
        let detail = parse_provider_error(body);
        assert_eq!(detail.code.as_deref(), Some("invalid_api_key"));
        let reason = detail
            .message
            .clone()
            .unwrap_or_else(|| truncate(body, 500));
        let code = detail.code.clone().expect("structured code");
        let error = ModelError::from_status_and_code(400, reason, Some(code.as_str()))
            .with_provider_code(code);
        assert_eq!(error.kind, leveler_model::ModelErrorKind::Auth);
        assert_eq!(error.provider_code(), Some("invalid_api_key"));
    }

    /// A genuine invalid request with an unrelated structured code stays a
    /// request error — the auth refinement must not swallow it.
    #[test]
    fn a_non_auth_structured_code_keeps_the_request_classification() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"bad tool schema"}}"#;
        let detail = parse_provider_error(body);
        let error = ModelError::from_status_and_code(
            400,
            detail.message.clone().unwrap_or_default(),
            detail.code.as_deref(),
        );
        assert_eq!(error.kind, leveler_model::ModelErrorKind::InvalidRequest);
    }

    fn ctx(api_key: Option<&str>, extra: Vec<(String, String)>) -> ProtocolContext {
        ProtocolContext {
            base_url: "https://x".into(),
            model_id: "m".into(),
            api_key: api_key.map(String::from),
            extra_headers: extra,
            reasoning: leveler_model::ReasoningConfig::default(),
            parallel_tool_calls: true,
            supports_temperature: true,
            thinking_supports_forced_tool_choice: true,
            reasoning_replay: leveler_model::ReasoningReplayContract::NONE,
        }
    }

    #[test]
    fn bearer_auth_used_when_no_explicit_api_key_header() {
        let client = reqwest::Client::new();
        let req = build_request(
            &client,
            "https://x/y",
            &serde_json::json!({}),
            &ctx(Some("k"), vec![]),
            None,
        )
        .build()
        .unwrap();
        assert_eq!(req.headers().get("authorization").unwrap(), "Bearer k");
        assert!(req.headers().get("x-api-key").is_none());
    }

    #[test]
    fn explicit_x_api_key_suppresses_bearer_auth() {
        let client = reqwest::Client::new();
        let extra = vec![
            ("x-api-key".to_string(), "k".to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ];
        let req = build_request(
            &client,
            "https://x/y",
            &serde_json::json!({}),
            &ctx(Some("k"), extra),
            None,
        )
        .build()
        .unwrap();
        assert!(
            req.headers().get("authorization").is_none(),
            "bearer must be suppressed when x-api-key is explicit"
        );
        assert_eq!(req.headers().get("x-api-key").unwrap(), "k");
        assert_eq!(
            req.headers().get("anthropic-version").unwrap(),
            "2023-06-01"
        );
    }

    /// A caller that owns a deadline spends it down across attempts; one that
    /// does not keeps the provider default on every attempt, exactly as before.
    #[test]
    fn a_deadline_shrinks_across_attempts_and_a_default_does_not() {
        let default = RequestBudget::PerRequest(Some(Duration::from_secs(120)));
        assert_eq!(
            default.remaining(),
            Some(Some(Duration::from_secs(120))),
            "no caller budget: the configured timeout applies to each attempt"
        );

        // The budget is seconds, not milliseconds, on purpose: the property
        // under test is that time spent is deducted, and a loaded runner can
        // lose a couple of hundred milliseconds to scheduling between these
        // two reads. A tight budget turns that into a flake about nothing.
        let deadline = RequestBudget::Deadline(Instant::now() + Duration::from_secs(5));
        let first = deadline.remaining().expect("budget left").expect("bounded");
        std::thread::sleep(Duration::from_millis(50));
        let second = deadline.remaining().expect("budget left").expect("bounded");
        assert!(
            second < first,
            "a retry may only spend what is left: {first:?} then {second:?}"
        );
        assert!(
            second <= Duration::from_millis(4_960),
            "and not a fresh full budget: {second:?}"
        );
    }

    /// A refused socket is classified from the structured OS error, not from
    /// the rendered message. The port is obtained by binding and immediately
    /// dropping a listener, so it is guaranteed closed and local.
    #[tokio::test]
    async fn a_refused_connection_carries_a_structured_fault() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let client = reqwest::Client::builder()
            // Generous: the refused connection is refused immediately, and a
            // loaded CI runner must not turn that refusal into a connect
            // timeout, which is a different structured kind.
            .connect_timeout(Duration::from_secs(30))
            .build()
            .expect("client");
        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect_err("nothing listens on the dropped port");
        let mapped = map_reqwest_error(&err);
        assert_eq!(mapped.kind, ModelErrorKind::ProviderUnavailable);
        assert_eq!(mapped.delivery_state, DeliveryState::NotSent);
        assert_eq!(
            mapped.transport_fault(),
            Some(leveler_model::TransportFault::ConnectionRefused),
            "the fault must come from io::ErrorKind, not the message \
             (observed {:?}): {err}",
            connect_io_kind(&err)
        );
    }

    #[test]
    fn an_elapsed_deadline_permits_no_request() {
        let spent = RequestBudget::Deadline(Instant::now() - Duration::from_secs(1));
        assert!(spent.remaining().is_none());
    }
}
