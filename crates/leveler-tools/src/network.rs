//! Host HTTP socket mediation. Confined calls enforce their admitted scope;
//! frozen FullAccess authority bypasses destination restrictions.
use leveler_execution::NetworkScope;
use std::time::{Duration, Instant};

use crate::network_budget::{NetworkBudget, NetworkLease, NetworkTrace, RequestClass};

/// Identity and diagnostic handles one outbound request carries. The class
/// chooses the provider budget; the session shares the process-fairness
/// layer; the optional trace records budget waits for the executor.
#[derive(Clone, Copy)]
pub(crate) struct NetworkCall<'a> {
    pub class: RequestClass<'a>,
    pub session: Option<&'a str>,
    pub trace: Option<&'a NetworkTrace>,
}

/// Only the stage currently being polled can be timed. A dropped future
/// (e.g. interrupted by an outer select) cannot report a completed stage.
pub(crate) struct WebPhase {
    started: Option<Instant>,
    phase: &'static str,
    hop: Option<usize>,
    http_status: Option<u16>,
}

pub(crate) enum PhaseOutcome {
    Ok,
    Error,
    Timeout,
    Limit,
    Interrupted,
}

impl PhaseOutcome {
    pub(crate) fn request_error(error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else {
            Self::Error
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Limit => "limit",
            Self::Interrupted => "interrupted",
        }
    }
}

impl WebPhase {
    pub(crate) fn body(status: reqwest::StatusCode) -> Self {
        Self::new("http_body", None, Some(status.as_u16()))
    }

    fn new(phase: &'static str, hop: Option<usize>, http_status: Option<u16>) -> Self {
        Self {
            started: tracing::enabled!(target: "leveler_web_latency", tracing::Level::DEBUG)
                .then(Instant::now),
            phase,
            hop,
            http_status,
        }
    }

    pub(crate) fn finish(mut self, outcome: PhaseOutcome) {
        self.record(outcome);
    }

    fn record(&mut self, outcome: PhaseOutcome) {
        if let Some(started) = self.started.take() {
            tracing::debug!(
                target: "leveler_web_latency",
                phase = self.phase,
                elapsed_us = started.elapsed().as_micros() as u64,
                outcome = outcome.as_str(),
                hop = self.hop,
                http_status = self.http_status,
                "web HTTP phase"
            );
        }
    }
}

impl Drop for WebPhase {
    fn drop(&mut self) {
        self.record(PhaseOutcome::Interrupted);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum DnsPhase {
    AdmissionReason,
    AdmissionResource,
    HttpHop(usize),
}

impl DnsPhase {
    fn timer(self) -> WebPhase {
        match self {
            Self::AdmissionReason => WebPhase::new("admission_reason_dns", None, None),
            Self::AdmissionResource => WebPhase::new("admission_resource_dns", None, None),
            Self::HttpHop(hop) => WebPhase::new("http_dns", Some(hop), None),
        }
    }
}

/// Resolve once for admission evidence or the actual next HTTP hop.
pub(crate) async fn resolve_target(
    url: &str,
    phase: DnsPhase,
) -> Result<leveler_execution::NetworkResource, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("unsupported scheme (only http/https)".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URL credentials are not supported".into());
    }
    let host = parsed.host_str().ok_or("url missing host")?;
    let port = parsed.port_or_known_default().ok_or("url missing port")?;
    #[cfg(debug_assertions)]
    if let Some(addresses) = test_dns_rebind(host, port, phase) {
        return leveler_execution::NetworkResource::from_resolved(
            host,
            port,
            leveler_execution::NetworkTransport::Tcp,
            addresses,
        )
        .map_err(|e| e.to_string());
    }
    let timer = phase.timer();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        leveler_execution::resolve_network_resource(
            host,
            port,
            leveler_execution::NetworkTransport::Tcp,
        ),
    )
    .await;
    timer.finish(match &result {
        Ok(Ok(_)) => PhaseOutcome::Ok,
        Ok(Err(_)) => PhaseOutcome::Error,
        Err(_) => PhaseOutcome::Timeout,
    });
    result
        .map_err(|_| "DNS resolution timed out".to_string())?
        .map_err(|e| e.to_string())
}

/// A closed-set DNS answer for the address-revalidation test.
///
/// A test needs one endpoint that admission sees as an allowed address and
/// execution sees as a forbidden one. Public DNS cannot be scripted
/// deterministically, so this hook answers exactly one reserved host,
/// `rebind.leveler.test`, and only when the test installs it. It mirrors the
/// existing `leveler_core::test_crash` failpoint: debug builds only, a fixed
/// closed set with no model/repository input, and no effect in a release
/// build. It changes no validation: the real scope, SSRF and redirect checks
/// still run on whatever answer comes back.
#[cfg(debug_assertions)]
fn test_dns_rebind(host: &str, port: u16, phase: DnsPhase) -> Option<Vec<std::net::SocketAddr>> {
    const REBIND_HOST: &str = "rebind.leveler.test";
    if host != REBIND_HOST {
        return None;
    }
    let blocked = {
        let slot = test_dns_rebind_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (*slot)?
    };
    let address = match phase {
        // Admission sees a loopback destination, which the Internet scope
        // admits and the executor may execute without consent.
        DnsPhase::AdmissionReason | DnsPhase::AdmissionResource => {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
        // The hop itself sees the forbidden answer.
        DnsPhase::HttpHop(_) => blocked,
    };
    Some(vec![std::net::SocketAddr::new(address, port)])
}

#[cfg(debug_assertions)]
fn test_dns_rebind_slot() -> &'static std::sync::Mutex<Option<std::net::IpAddr>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<std::net::IpAddr>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Install the forbidden answer the next execution hop receives for
/// `rebind.leveler.test`. Debug builds only; the release build has no such
/// symbol. Used by the address-revalidation integration test.
#[cfg(debug_assertions)]
pub fn install_test_dns_rebind_for_tests(blocked: std::net::IpAddr) {
    *test_dns_rebind_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(blocked);
}

/// Remove the test answer. Safe to call when none was installed.
#[cfg(debug_assertions)]
pub fn clear_test_dns_rebind_for_tests() {
    *test_dns_rebind_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// A client is built for one validated hop only. It cannot discover an
/// environment proxy, resolve a second DNS answer, or follow a redirect itself.
///
/// The returned lease keeps the request's process/session/provider budget
/// while the caller reads the response body, so a slow body still counts as an
/// in-flight request.
pub(crate) async fn send(
    scope: &NetworkScope,
    unrestricted: bool,
    url: &str,
    method: reqwest::Method,
    json: Option<&serde_json::Value>,
    bearer: Option<&str>,
    timeout: Duration,
    call: NetworkCall<'_>,
) -> Result<(reqwest::Response, NetworkLease), String> {
    let budget = NetworkBudget::global();
    let logical = budget
        .acquire_logical(call.class, call.session, call.trace)
        .await;
    // FullAccess is frozen execution authority, not a larger destination scope.
    // Let the HTTP client perform ordinary DNS/connect/redirect processing;
    // no CodeLeveler scope or SSRF decision runs on this path. A request that
    // carries a credential or a JSON body still refuses redirects: following
    // one could resend the Authorization header to another host.
    if unrestricted {
        let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
        let host = parsed.host_str().ok_or("url missing host")?;
        let port = parsed.port_or_known_default().ok_or("url missing port")?;
        let host_permit = budget.acquire_host(host, port, call.trace).await;
        let redirects = if bearer.is_some() || json.is_some() {
            reqwest::redirect::Policy::none()
        } else {
            reqwest::redirect::Policy::limited(10)
        };
        let builder = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(redirects);
        let response = send_to_headers(builder, method, url, json, bearer, None).await?;
        return Ok((response, logical.attach_host(host_permit)));
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
        let resource = resolve_target(current.as_str(), DnsPhase::HttpHop(hop)).await?;
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
        let builder = reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(&resource.host, &resource.resolved_addresses)
            .user_agent(concat!("CodeLeveler/", env!("CARGO_PKG_VERSION")));
        // One hop holds one destination permit. A redirect releases it before
        // the next host is contacted; the request-wide layers stay held.
        let host_permit = budget
            .acquire_host(&resource.host, resource.port, call.trace)
            .await;
        let response = send_to_headers(
            builder,
            method.clone(),
            current.clone(),
            json,
            bearer,
            Some(hop),
        )
        .await?;
        if !response.status().is_redirection() {
            return Ok((response, logical.attach_host(host_permit)));
        }
        drop(host_permit);
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

async fn send_to_headers(
    builder: reqwest::ClientBuilder,
    method: reqwest::Method,
    url: impl reqwest::IntoUrl,
    json: Option<&serde_json::Value>,
    bearer: Option<&str>,
    hop: Option<usize>,
) -> Result<reqwest::Response, String> {
    let prepare = WebPhase::new("http_prepare", hop, None);
    let client = match builder.build() {
        Ok(client) => client,
        Err(error) => {
            prepare.finish(PhaseOutcome::Error);
            return Err(format!("http client: {error}"));
        }
    };
    let mut request = client.request(method, url);
    if let Some(body) = json {
        request = request.json(body);
    }
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    prepare.finish(PhaseOutcome::Ok);

    // Reqwest exposes response headers as one boundary. This interval
    // includes connection work and the server response where applicable.
    let mut send = WebPhase::new("http_to_headers", hop, None);
    let response = request.send().await;
    send.http_status = response.as_ref().ok().map(|value| value.status().as_u16());
    send.finish(match &response {
        Ok(_) => PhaseOutcome::Ok,
        Err(error) => PhaseOutcome::request_error(error),
    });
    response.map_err(request_error)
}

/// Preserve the one classification callers need from a reqwest failure. The
/// display text alone does not say "timed out" on every platform, so a search
/// or fetch could not distinguish a timeout from an unreachable host.
fn request_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "request failed: timed out".to_string()
    } else {
        format!("request failed: {error}")
    }
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

    fn call() -> NetworkCall<'static> {
        NetworkCall {
            class: RequestClass::Fetch,
            session: None,
            trace: None,
        }
    }

    async fn get(
        scope: &NetworkScope,
        url: &str,
    ) -> Result<(reqwest::Response, NetworkLease), String> {
        send(
            scope,
            false,
            url,
            reqwest::Method::GET,
            None,
            None,
            Duration::from_secs(2),
            call(),
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
                call(),
            )
            .await;
            if response.is_err() {
                task.abort();
            }
            let (response, _lease) = response.unwrap();
            assert_eq!(response.text().await.unwrap(), "ok");
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
            let (response, _lease) = result.unwrap();
            assert_eq!(response.text().await.unwrap(), "ok");
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
        let (response, _lease) = result.unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
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

    /// V2 Auto's `Any` scope carries ordinary private/LAN reachability. The
    /// public-document SSRF exclusion belongs to `Internet` alone; `Any` still
    /// validates the destination shape, but does not reject a private address
    /// before the socket attempt.
    #[tokio::test]
    async fn any_scope_reaches_private_destinations_without_the_public_ssrf_fence() {
        let internet = send(
            &NetworkScope::Internet,
            false,
            "http://192.168.1.10/",
            reqwest::Method::GET,
            None,
            None,
            Duration::from_millis(200),
            call(),
        )
        .await
        .unwrap_err();
        assert!(internet.contains("blocked private/loopback"), "{internet}");

        let any = send(
            &NetworkScope::Any,
            false,
            "http://192.168.1.10/",
            reqwest::Method::GET,
            None,
            None,
            Duration::from_millis(100),
            call(),
        )
        .await;
        if let Err(error) = any {
            assert!(!error.contains("blocked private/loopback"), "{error}");
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
