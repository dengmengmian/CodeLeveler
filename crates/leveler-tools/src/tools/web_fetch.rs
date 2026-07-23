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
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
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

/// Synchronous first-gate: scheme, `localhost`, and any *literal* IP in the
/// host — the checks that need no DNS. Hostnames pass here and get their
/// DNS-resolved addresses validated in [`resolve_and_validate`].
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
    if let Some(ip) = parse_host_ip(host)
        && is_blocked_ip(ip)
    {
        return Err(format!("blocked address: {ip}"));
    }
    Ok(())
}

/// Parse a URL host as a literal IP, tolerating the `[...]` brackets a URL puts
/// around IPv6 literals (`[::1]`). `None` for a real domain name.
fn parse_host_ip(host: &str) -> Option<IpAddr> {
    let inner = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    inner.parse::<IpAddr>().ok()
}

/// A validated fetch target: the URL's host plus the exact public-routable IPs
/// it resolved to. Pinning the connection to these addresses (instead of letting
/// the HTTP client re-resolve the hostname) closes the DNS-rebinding window
/// between our validation and the actual connect.
pub(crate) struct SafeTarget {
    host: String,
    /// Empty for a literal-IP host — there is no DNS name to pin.
    addrs: Vec<SocketAddr>,
}

/// Full validation: the sync gate above, then (for a hostname) resolve every
/// A/AAAA and fail closed if any is private/link-local/metadata. DNS runs off
/// the async worker so a slow resolver can't stall the executor.
pub(crate) async fn resolve_and_validate(url: &str) -> Result<SafeTarget, String> {
    assert_url_safe_for_fetch(url)?;
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "url missing host".to_string())?
        .to_string();
    if parse_host_ip(&host).is_some() {
        // Literal IP: already validated by the sync gate; nothing to resolve.
        return Ok(SafeTarget {
            host,
            addrs: Vec::new(),
        });
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let dns_host = host.clone();
    let addrs = resolve_with_timeout(
        async move {
            tokio::net::lookup_host((dns_host.as_str(), port))
                .await
                .map(|addresses| addresses.collect::<Vec<_>>())
        },
        DNS_TIMEOUT,
    )
    .await
    .map_err(|e| format!("dns resolve failed for `{host}`: {e}"))?;
    if addrs.is_empty() {
        return Err(format!("dns resolve returned no addresses for `{host}`"));
    }
    for addr in &addrs {
        if is_blocked_ip(addr.ip()) {
            return Err(format!("blocked address after resolve: {}", addr.ip()));
        }
    }
    Ok(SafeTarget { host, addrs })
}

async fn resolve_with_timeout<F>(future: F, timeout: Duration) -> Result<Vec<SocketAddr>, String>
where
    F: std::future::Future<Output = std::io::Result<Vec<SocketAddr>>>,
{
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs_f64()))?
        .map_err(|error| error.to_string())
}

/// True for loopback, private, link-local, and cloud metadata ranges.
pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
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
                || v6.is_multicast()
                // IPv4-mapped: re-check embedded v4
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(IpAddr::V4(v4)))
        }
    }
}

async fn fetch_url(url: &str, max_bytes: usize) -> Result<String, String> {
    let mut current = url.to_string();
    let mut hops = 0usize;
    loop {
        // Resolve + validate this hop, then pin the client to the validated IPs
        // so reqwest cannot re-resolve the hostname to a rebound internal
        // address between the check and the connect (DNS-rebinding SSRF).
        let target = resolve_and_validate(&current).await?;
        let mut builder = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(format!(
                "CodeLeveler-web_fetch/{}",
                env!("CARGO_PKG_VERSION")
            ));
        if !target.addrs.is_empty() {
            builder = builder.resolve_to_addrs(&target.host, &target.addrs);
        }
        let client = builder.build().map_err(|e| format!("http client: {e}"))?;

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
        let (mut body, truncated) = read_body_capped(resp, max_bytes).await?;
        if truncated {
            body.push_str(&format!("\n\n[web_fetch truncated at {max_bytes} bytes]"));
        }
        return Ok(format!("URL: {current}\n\n{body}"));
    }
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
    async fn dns_resolution_has_an_explicit_deadline() {
        let pending = std::future::pending::<std::io::Result<Vec<SocketAddr>>>();
        let error = resolve_with_timeout(pending, Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
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
    async fn resolve_and_validate_gate_and_pinning_for_literal_ips() {
        // A public literal IP validates with nothing to pin (no DNS name).
        let ok = resolve_and_validate("https://8.8.8.8/x").await.unwrap();
        assert!(ok.addrs.is_empty(), "literal IP needs no DNS pinning");
        // The async path must still fail closed on private / metadata / v6 loopback.
        assert!(
            resolve_and_validate("http://169.254.169.254/latest/meta-data")
                .await
                .is_err()
        );
        assert!(resolve_and_validate("http://[::1]/x").await.is_err());
        assert!(resolve_and_validate("http://10.0.0.1/x").await.is_err());
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
