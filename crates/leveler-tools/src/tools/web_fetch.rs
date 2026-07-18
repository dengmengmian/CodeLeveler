//! `web_fetch` — fetch a public HTTP(S) document into the agent context.
//!
//! No API key. SSRF-hardened: only http(s), blocks private/link-local/metadata
//! addresses (checked after DNS resolve and on each redirect hop). Output is
//! size-capped plain text.

use std::net::{IpAddr, SocketAddr};
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
const MAX_REDIRECTS: usize = 5;

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
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch a public HTTP(S) URL and return text content (docs, raw files). \
         Blocks private/link-local/metadata addresses (SSRF protection). \
         Prefer this over shell curl. Does not require a search API key."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        if context.deny_network {
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
            r = fetch_url(&url, max_bytes) => r,
        };

        match result {
            Ok(text) => Ok(ToolOutput::ok(text)),
            Err(reason) => Ok(ToolOutput::error(format!(
                "web_fetch 不可用:{reason}。请基于已有知识继续,或改用其他工具。"
            ))),
        }
    }
}

/// Validate scheme and that every resolved IP is public-routable.
pub(crate) fn assert_url_safe_for_fetch(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("unsupported scheme `{scheme}` (only http/https)"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "url missing host".to_string())?;
    if host.eq_ignore_ascii_case("localhost") {
        return Err("blocked host: localhost".to_string());
    }
    // Literal IP in host.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(format!("blocked address: {ip}"));
        }
        return Ok(());
    }
    // DNS resolve all A/AAAA and block if any is private (fail closed).
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs: Vec<SocketAddr> = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|e| format!("dns resolve failed for `{host}`: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("dns resolve returned no addresses for `{host}`"));
    }
    for addr in &addrs {
        if is_blocked_ip(addr.ip()) {
            return Err(format!("blocked address after resolve: {}", addr.ip()));
        }
    }
    Ok(())
}

use std::net::ToSocketAddrs;

/// True for loopback, private, link-local, and cloud metadata ranges.
pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
                // CGNAT 100.64.0.0/10
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
                // 169.254.0.0/16 already link_local; metadata 169.254.169.254 covered
                || v4.octets() == [169, 254, 169, 254]
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                // IPv4-mapped: re-check embedded v4
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(IpAddr::V4(v4)))
        }
    }
}

async fn fetch_url(url: &str, max_bytes: usize) -> Result<String, String> {
    assert_url_safe_for_fetch(url)?;

    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!(
            "CodeLeveler-web_fetch/{}",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let mut current = url.to_string();
    let mut hops = 0usize;
    loop {
        assert_url_safe_for_fetch(&current)?;
        let resp = client
            .get(&current)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        if status.is_redirection() {
            hops += 1;
            if hops > MAX_REDIRECTS {
                return Err(format!("too many redirects (>{MAX_REDIRECTS})"));
            }
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| "redirect without Location".to_string())?;
            let next = reqwest::Url::parse(&current)
                .and_then(|base| base.join(loc))
                .map_err(|e| format!("bad redirect: {e}"))?;
            current = next.to_string();
            continue;
        }
        if !status.is_success() {
            return Err(format!("HTTP {status} for {current}"));
        }
        let bytes = resp.bytes().await.map_err(|e| format!("read body: {e}"))?;
        let truncated = bytes.len() > max_bytes;
        let slice = if truncated {
            &bytes[..max_bytes]
        } else {
            &bytes
        };
        // Lossy UTF-8; binary still surfaces as replacement chars rather than panic.
        let mut body = String::from_utf8_lossy(slice).into_owned();
        if truncated {
            body.push_str(&format!(
                "\n\n[web_fetch truncated at {max_bytes} bytes; original {} bytes]",
                bytes.len()
            ));
        }
        return Ok(format!("URL: {current}\n\n{body}"));
    }
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
    fn rejects_non_http_schemes_and_localhost() {
        assert!(assert_url_safe_for_fetch("file:///etc/passwd").is_err());
        assert!(assert_url_safe_for_fetch("ftp://example.com/a").is_err());
        assert!(assert_url_safe_for_fetch("http://localhost/x").is_err());
        assert!(assert_url_safe_for_fetch("http://127.0.0.1/x").is_err());
        assert!(assert_url_safe_for_fetch("http://192.168.0.1/x").is_err());
        assert!(assert_url_safe_for_fetch("http://[::1]/x").is_err());
    }

    #[test]
    fn allows_public_literal_ip_shape() {
        // 8.8.8.8 is public; no DNS needed.
        assert!(assert_url_safe_for_fetch("https://8.8.8.8/resolve").is_ok());
    }

    #[tokio::test]
    async fn deny_network_returns_error_output() {
        let tool = WebFetchTool;
        let ws = leveler_execution::Workspace::new(std::env::temp_dir()).unwrap();
        let mut ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::Assisted);
        ctx.deny_network = true;
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
