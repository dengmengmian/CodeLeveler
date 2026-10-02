//! Host HTTP socket mediation. Confined calls enforce their admitted scope;
//! frozen FullAccess authority bypasses destination restrictions.
use leveler_execution::NetworkScope;
use std::time::Duration;

/// Resolve once for admission evidence or the actual next HTTP hop.
pub(crate) async fn resolve_target(
    url: &str,
) -> Result<leveler_execution::NetworkResource, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("unsupported scheme (only http/https)".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URL credentials are not supported".into());
    }
    let host = parsed.host_str().ok_or("url missing host")?;
    tokio::time::timeout(
        Duration::from_secs(5),
        leveler_execution::resolve_network_resource(
            host,
            parsed.port_or_known_default().ok_or("url missing port")?,
            leveler_execution::NetworkTransport::Tcp,
        ),
    )
    .await
    .map_err(|_| "DNS resolution timed out".to_string())?
    .map_err(|e| e.to_string())
}

/// A client is built for one validated hop only. It cannot discover an
/// environment proxy, resolve a second DNS answer, or follow a redirect itself.
pub(crate) async fn send(
    scope: &NetworkScope,
    unrestricted: bool,
    url: &str,
    method: reqwest::Method,
    json: Option<&serde_json::Value>,
    bearer: Option<&str>,
    timeout: Duration,
) -> Result<reqwest::Response, String> {
    // FullAccess is frozen execution authority, not a larger destination scope.
    // Let the HTTP client perform ordinary DNS/connect/redirect processing;
    // no CodeLeveler scope or SSRF decision runs on this path.
    if unrestricted {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let mut request = client.request(method, url);
        if let Some(body) = json {
            request = request.json(body);
        }
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        return request
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"));
    }
    let mut current = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    let mut effective_scope = scope.clone();
    for hop in 0..=5 {
        if !matches!(current.scheme(), "http" | "https") {
            return Err("unsupported scheme (only http/https)".into());
        }
        if !current.username().is_empty() || current.password().is_some() {
            return Err("URL credentials are not supported".into());
        }
        if matches!(scope, NetworkScope::None) {
            return Err("network scope None denies destination".into());
        }
        let resource = resolve_target(current.as_str()).await?;
        // Auto's ordinary local HTTP destination stays local for the whole
        // call. A public initial target never acquires this authority through
        // a redirect, and a localhost redirect cannot widen into LAN/public.
        if hop == 0
            && matches!(scope, NetworkScope::Internet)
            && NetworkScope::Loopback.validate_resource(&resource).is_ok()
        {
            effective_scope = NetworkScope::Loopback;
        }
        effective_scope
            .validate_resource(&resource)
            .map_err(|e| e.to_string())?;
        // Preserve the public-document SSRF contract under Internet authority.
        // Non-loopback private addresses require explicit local authority.
        if matches!(effective_scope, NetworkScope::Internet)
            && resource
                .resolved_addresses
                .iter()
                .any(|a| is_blocked_ip(a.ip()))
        {
            return Err("blocked private/loopback address under Internet HTTP scope".into());
        }
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(&resource.host, &resource.resolved_addresses)
            .user_agent(concat!("CodeLeveler/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let mut request = client.request(method.clone(), current.clone());
        if let Some(body) = json {
            request = request.json(body);
        }
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        if !response.status().is_redirection() {
            return Ok(response);
        }
        if hop == 5 {
            return Err("too many redirects (>5)".into());
        }
        // Never resend a search query or bearer credential to a redirect target.
        if method != reqwest::Method::GET || json.is_some() || bearer.is_some() {
            return Err("authenticated or non-GET HTTP redirects are not supported".into());
        }
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or("redirect without Location")?;
        current = current
            .join(location)
            .map_err(|e| format!("bad redirect: {e}"))?;
    }
    Err("too many redirects".into())
}

/// SSRF exclusions for public HTTP documents; explicit local scopes use the
/// execution contract instead. IPv4-mapped IPv6 follows the embedded IPv4 class.
pub(crate) fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(v4.into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn server(ip: &str, location: Option<String>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0u8; 2048];
            assert!(stream.read(&mut bytes).await.unwrap() > 0);
            let response = match location {
                Some(url) => format!(
                    "HTTP/1.1 302 Found\r\nLocation: {url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
                None => {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
                }
            };
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{address}/"), task)
    }

    async fn get(scope: &NetworkScope, url: &str) -> Result<reqwest::Response, String> {
        send(
            scope,
            false,
            url,
            reqwest::Method::GET,
            None,
            None,
            Duration::from_secs(2),
        )
        .await
    }

    #[tokio::test]
    async fn unrestricted_http_ignores_none_and_internet_ssrf_boundaries() {
        for scope in [NetworkScope::None, NetworkScope::Internet] {
            let (url, task) = server("127.0.0.1", None).await;
            let response = send(
                &scope,
                true,
                &url,
                reqwest::Method::GET,
                None,
                None,
                Duration::from_secs(2),
            )
            .await;
            if response.is_err() {
                task.abort();
            }
            assert_eq!(response.unwrap().text().await.unwrap(), "ok");
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn loopback_ipv4_and_localhost_reach_real_server() {
        for host in ["127.0.0.1", "localhost"] {
            let (url, task) = server("127.0.0.1", None).await;
            let url = url.replace("127.0.0.1", host);
            let result = get(&NetworkScope::Loopback, &url).await;
            if result.is_err() {
                task.abort();
            }
            assert_eq!(result.unwrap().text().await.unwrap(), "ok");
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn loopback_ipv6_reaches_real_server() {
        let (url, task) = server("::1", None).await;
        let result = get(&NetworkScope::Loopback, &url).await;
        if result.is_err() {
            task.abort();
        }
        assert_eq!(result.unwrap().text().await.unwrap(), "ok");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn loopback_rejects_public_and_lan_before_socket_connection() {
        for url in [
            "http://8.8.8.8/",
            "http://192.168.1.10/",
            "http://10.0.0.1/",
        ] {
            let error = get(&NetworkScope::Loopback, url).await.unwrap_err();
            assert!(error.contains("scope"), "{error}");
        }
    }

    #[tokio::test]
    async fn internet_local_start_stays_loopback_across_redirects() {
        for target in ["http://8.8.8.8/", "http://192.168.1.10/"] {
            let (url, task) = server("127.0.0.1", Some(target.into())).await;
            let error = get(&NetworkScope::Internet, &url).await.unwrap_err();
            task.await.unwrap();
            assert!(error.contains("scope"), "{error}");
        }
    }

    #[tokio::test]
    async fn redirect_cannot_escape_loopback() {
        let (url, task) = server("127.0.0.1", Some("http://8.8.8.8/".into())).await;
        let error = get(&NetworkScope::Loopback, &url).await.unwrap_err();
        task.await.unwrap();
        assert!(error.contains("scope"), "{error}");
    }

    #[tokio::test]
    async fn none_rejects_live_localhost_server() {
        let (url, task) = server("127.0.0.1", None).await;
        let error = get(&NetworkScope::None, &url).await.unwrap_err();
        task.abort();
        assert!(error.contains("scope"), "{error}");
    }
}
