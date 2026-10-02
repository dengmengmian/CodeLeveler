//! `web_fetch` — fetch a public HTTP(S) document into the agent context.
//!
//! No API key. Host socket mediation enforces the admitted scope on pinned DNS
//! addresses and every redirect hop. Internet requests retain SSRF exclusions;
//! explicit local scopes permit development endpoints. FullAccess bypasses
//! destination/SSRF restrictions. Output is size-capped.

#[cfg(test)]
use crate::network::is_blocked_ip;
#[cfg(test)]
use std::net::IpAddr;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_MAX_BYTES: usize = 512 * 1024;
const HARD_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Absolute http(s) URL to fetch.
    url: String,
    /// Max response body bytes (default 512 KiB, hard cap 2 MiB).
    #[serde(default)]
    max_bytes: Option<usize>,
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn requires_workspace(&self) -> bool {
        false
    }
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch an HTTP or HTTPS URL and return its text. Loopback URLs support local development; other private destinations require local network authority or approval. Fails when network is denied for \
         this turn. Does not use a search API key. Optional `max_bytes` caps \
         the returned body."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }

    async fn admission_reason(
        &self,
        input: &serde_json::Value,
        context: &ToolContext,
    ) -> Option<String> {
        if context.policy.unrestricted_execution()
            || !matches!(
                context.policy.network_scope(),
                leveler_execution::NetworkScope::Internet
            )
        {
            return None;
        }
        let url = input.get("url")?.as_str()?;
        let resource = crate::network::resolve_target(url.trim()).await.ok()?;
        let loopback = leveler_execution::NetworkScope::Loopback
            .validate_resource(&resource)
            .is_ok();
        (!loopback
            && resource
                .resolved_addresses
                .iter()
                .any(|address| crate::network::is_blocked_ip(address.ip())))
        .then(|| "HTTP access to LAN/private destinations requires explicit approval".to_string())
    }

    async fn network_resource(
        &self,
        input: &serde_json::Value,
    ) -> Result<Option<leveler_execution::NetworkResource>, String> {
        let url = input
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or("web_fetch needs a URL")?;
        crate::network::resolve_target(url.trim()).await.map(Some)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let network_scope = context.policy.network_scope();
        let unrestricted = context.policy.unrestricted_execution();
        if !unrestricted && matches!(network_scope, leveler_execution::NetworkScope::None) {
            return Ok(ToolOutput::error(
                "web_fetch 不可用:当前模式/沙箱已禁用网络。",
            ));
        }
        let url = input.url.trim().to_string();
        if url.is_empty() {
            return Ok(ToolOutput::error("web_fetch 需要非空 url。"));
        }
        let max_bytes = input
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(1, HARD_MAX_BYTES);

        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Ok(ToolOutput::error("web_fetch 已取消。"));
            }
            r = fetch_url(&url, max_bytes, &network_scope, unrestricted) => r,
        };

        match result {
            Ok(text) => Ok(ToolOutput::ok(text)),
            Err(reason) => Ok(ToolOutput::error(format!(
                "web_fetch 不可用:{reason}。请基于已有知识继续,或改用其他工具。"
            ))),
        }
    }
}

async fn fetch_url(
    url: &str,
    max_bytes: usize,
    scope: &leveler_execution::NetworkScope,
    unrestricted: bool,
) -> Result<String, String> {
    let response = crate::network::send(
        scope,
        unrestricted,
        url,
        reqwest::Method::GET,
        None,
        None,
        TIMEOUT,
    )
    .await?;
    let status = response.status();
    let final_url = response.url().to_string();
    if !status.is_success() {
        return Err(format!("HTTP {status} for {final_url}"));
    }
    let (mut body, truncated) = read_body_capped(response, max_bytes).await?;
    if truncated {
        body.push_str(&format!("\n\n[web_fetch truncated at {max_bytes} bytes]"));
    }
    Ok(format!("URL: {final_url}\n\n{body}"))
}

/// Read the response body a chunk at a time, stopping once `max_bytes` are
/// collected. Bounds memory regardless of what the server sends (or claims in
/// `Content-Length`). Returns the decoded text and whether it was cut short.
async fn read_body_capped(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> Result<(String, bool), String> {
    let mut collected: Vec<u8> = Vec::new();
    let limit = max_bytes.saturating_add(1);
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("read body: {e}"))? {
        let remaining = limit.saturating_sub(collected.len());
        if chunk.len() > remaining {
            collected.extend_from_slice(&chunk[..remaining]);
            break;
        }
        collected.extend_from_slice(&chunk);
        if collected.len() == limit {
            break;
        }
    }
    let truncated = collected.len() > max_bytes;
    collected.truncate(max_bytes);
    // Lossy UTF-8; binary surfaces as replacement chars rather than panicking.
    Ok((String::from_utf8_lossy(&collected).into_owned(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn blocks_private_and_loopback_v4() {
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_blocked_ip(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn blocks_loopback_and_ula_v6() {
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // fc00::/7 unique local
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::new(
            0xfc00, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[test]
    fn blocks_multicast_destinations() {
        assert!(is_blocked_ip(IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1))));
        assert!(is_blocked_ip(IpAddr::V6(Ipv6Addr::new(
            0xff02, 0, 0, 0, 0, 0, 0, 1
        ))));
    }

    #[tokio::test]
    async fn exact_size_body_is_not_reported_as_truncated() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n12345")
                .await
                .unwrap();
        });
        let response = reqwest::get(format!("http://{addr}")).await.unwrap();
        let (body, truncated) = read_body_capped(response, 5).await.unwrap();
        server.await.unwrap();

        assert_eq!(body, "12345");
        assert!(!truncated, "an exact-boundary response was not cut short");
    }

    #[tokio::test]
    async fn loopback_scope_tool_fetches_live_localhost() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0u8; 1024];
            assert!(stream.read(&mut bytes).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let output = WebFetchTool
            .execute(
                serde_json::json!({"url": format!("http://localhost:{}/health", address.port())}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        if output.is_error {
            server.abort();
        }
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.ends_with("ok"));
        server.await.unwrap();
        for url in ["http://8.8.8.8/", "http://192.168.1.10/"] {
            let denied = WebFetchTool
                .execute(
                    serde_json::json!({"url": url}),
                    ctx.clone(),
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            assert!(denied.is_error);
            assert!(denied.content.contains("scope"), "{}", denied.content);
        }
    }

    #[tokio::test]
    async fn auto_private_http_requires_consent_but_loopback_and_public_do_not() {
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        for url in [
            "http://192.168.1.10/",
            "http://10.0.0.1/",
            "http://169.254.169.254/",
        ] {
            let reason = WebFetchTool
                .admission_reason(&serde_json::json!({"url": url}), &ctx)
                .await;
            assert!(reason.is_some(), "{url} must request consent");
        }
        for url in ["http://127.0.0.1/", "http://[::1]/", "http://8.8.8.8/"] {
            assert!(
                WebFetchTool
                    .admission_reason(&serde_json::json!({"url": url}), &ctx)
                    .await
                    .is_none(),
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn auto_internet_scope_fetches_live_localhost_without_approval() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0u8; 1024];
            assert!(stream.read(&mut bytes).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        let out = WebFetchTool
            .execute(
                serde_json::json!({"url": format!("http://localhost:{}/health", address.port())}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        if out.is_error {
            server.abort();
        }
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.ends_with("ok"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn full_access_tool_fetches_localhost_despite_explicit_network_denial() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0u8; 1024];
            assert!(stream.read(&mut bytes).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
        });
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::FullAccess)
            .with_sandbox(true);
        let output = WebFetchTool
            .execute(
                serde_json::json!({"url": format!("http://localhost:{}/health", address.port())}),
                ctx.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        if output.is_error {
            server.abort();
        }
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.ends_with("ok"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn deny_network_returns_error_output() {
        let tool = WebFetchTool;
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let mut ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        ctx = ctx.with_sandbox(true);
        let out = tool
            .execute(
                serde_json::json!({"url": "https://example.com"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("禁用网络"), "{}", out.content);
    }
}
