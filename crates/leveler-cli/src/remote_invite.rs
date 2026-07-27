//! `/remote` from inside the TUI: one command, a QR, and a fingerprint to
//! compare.
//!
//! The pieces already existed as separate CLI commands — enable, enroll, agent,
//! pair, confirm — and a person had to run five of them in the right order with
//! the right environment. That is a workflow for someone who wrote it. What a
//! user should do is show their phone a code.
//!
//! Two things this makes different from the command sequence:
//!
//! - **The agent runs in this process**, over the runtime the TUI already
//!   holds. No second process, no service to install, no daemon that outlives
//!   the window someone closed. It is reachable exactly as long as the session
//!   they are looking at.
//! - **`/xremote-loc` binds the relay to this machine's own address.** A phone on
//!   the same Wi-Fi reaches it directly, so the whole thing works with no server
//!   anywhere. Without it, remote control needs a relay someone has to run.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{Context as _, bail};
use leveler_local_transport::LocalRuntimeService;
use leveler_remote_agent::{
    AgentBridge, AuditLog, RemoteConfig, RemoteHome, SingleProject, TrustedDevices, run_tunnel,
    runtime_id_for,
};
use leveler_remote_protocol::VerifyingKey;
use leveler_remote_protocol::auth::{RUNTIME_AUTH_HEADER, RuntimeAssertion, runtime_action};
use leveler_remote_protocol::pairing::PairingQrPayload;
use leveler_tui::{PairingRequest, RemoteInvite, RemoteOutcome, RemoteRequest};

/// Everything one `/remote` session needs to remember between requests.
#[derive(Default)]
pub(crate) struct RemoteSession {
    /// Set once the host side is up, so a second `/remote` reuses it instead of
    /// starting a second relay and orphaning the first.
    started: bool,
    pairing_id: Option<String>,
}

/// Render a QR as terminal rows.
///
/// Two modules per character cell using half blocks: a QR is square, terminal
/// cells are not, and a code drawn one module per cell is twice as tall as it
/// should be — on most terminals that means it does not fit on screen, which
/// makes it a QR nobody can scan.
fn qr_rows(payload: &str) -> anyhow::Result<Vec<String>> {
    use qrcode::{EcLevel, QrCode};

    // Low correction keeps the code small; the screen is clean and static, not
    // a sticker on a wall.
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L)
        .context("配对载荷太长，无法编码成二维码")?;
    let width = code.width();
    let modules = code.to_colors();
    let dark = |x: usize, y: usize| -> bool {
        if x >= width || y >= width {
            return false;
        }
        modules[y * width + x] == qrcode::Color::Dark
    };

    // A quiet zone of four modules on every side; scanners need it.
    const QUIET: usize = 4;
    let span = width + QUIET * 2;
    let mut rows = Vec::new();
    let mut y = 0;
    while y < span {
        let mut row = String::with_capacity(span);
        for x in 0..span {
            let top = dark(x.wrapping_sub(QUIET), y.wrapping_sub(QUIET));
            let bottom = dark(x.wrapping_sub(QUIET), (y + 1).wrapping_sub(QUIET));
            // Inverted on purpose: terminals are usually dark, and a scanner
            // needs dark modules on a light field. Drawing the *background* as
            // the light field with block glyphs gives that on either theme.
            row.push(match (top, bottom) {
                (true, true) => ' ',
                (true, false) => '▄',
                (false, true) => '▀',
                (false, false) => '█',
            });
        }
        rows.push(row);
        y += 2;
    }
    Ok(rows)
}

/// This machine's address on the local network, for `/xremote-loc`.
///
/// Loopback is useless here: the phone is a different machine, and `127.0.0.1`
/// on a phone is the phone.
fn lan_address() -> anyhow::Result<IpAddr> {
    // Ask the routing table which source address would be used to reach the
    // outside, without sending anything: a connected UDP socket resolves the
    // route locally.
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").context("无法探测本机网络地址")?;
    socket
        .connect("223.5.5.5:80")
        .context("无法探测本机网络地址（网络不可达？）")?;
    let address = socket.local_addr().context("无法探测本机网络地址")?.ip();
    if address.is_loopback() {
        bail!("只探测到回环地址，手机无法连到这台机器");
    }
    Ok(address)
}

fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn nonce() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("system RNG is available");
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn auth_header(
    key: &leveler_remote_protocol::SigningKey,
    action: &str,
    runtime_id: &str,
) -> String {
    RuntimeAssertion::header_value(key, action, runtime_id, &now_stamp(), &nonce())
}

/// Build the closure the TUI calls.
pub(crate) fn launcher(
    runtime: Arc<dyn LocalRuntimeService>,
    repo_root: std::path::PathBuf,
    home: RemoteHome,
) -> leveler_tui::RemoteLauncher {
    let session: Arc<Mutex<RemoteSession>> = Arc::new(Mutex::new(RemoteSession::default()));
    Arc::new(move |request| {
        let runtime = runtime.clone();
        let repo_root = repo_root.clone();
        let home = home.clone();
        let session = session.clone();
        Box::pin(async move {
            match handle(request, runtime, repo_root, home, session).await {
                Ok(outcome) => outcome,
                Err(error) => RemoteOutcome::Failed(format!("{error}")),
            }
        })
    })
}

async fn handle(
    request: RemoteRequest,
    runtime: Arc<dyn LocalRuntimeService>,
    repo_root: std::path::PathBuf,
    home: RemoteHome,
    session: Arc<Mutex<RemoteSession>>,
) -> anyhow::Result<RemoteOutcome> {
    match request {
        RemoteRequest::Invite { local } => invite(runtime, repo_root, home, session, local).await,
        RemoteRequest::Pending => pending(&home, &session).await,
        RemoteRequest::Accept => decide(&home, &session, true).await,
        RemoteRequest::Reject => decide(&home, &session, false).await,
    }
}

/// Bring the host side up (once) and produce an invite.
async fn invite(
    runtime: Arc<dyn LocalRuntimeService>,
    repo_root: std::path::PathBuf,
    home: RemoteHome,
    session: Arc<Mutex<RemoteSession>>,
    local: bool,
) -> anyhow::Result<RemoteOutcome> {
    let key = match home.load_key()? {
        Some(key) => key,
        None => home.create_key()?,
    };
    let runtime_id = runtime_id_for(&key.verifying_key());

    let mut config = home.load_config()?.unwrap_or_else(|| {
        RemoteConfig::new(
            "http://127.0.0.1:18443",
            default_display_name(),
            runtime_id.clone(),
        )
    });
    config.runtime_id = runtime_id.clone();

    let already_started = { session.lock().unwrap().started };
    if local && !already_started {
        // Own relay, own machine, own network. Nothing leaves the LAN.
        let address = lan_address()?;
        let bind: SocketAddr = SocketAddr::new(IpAddr::from([0, 0, 0, 0]), 18443);
        let secret = nonce();
        let state = leveler_relay::RelayState::with_enrollment_secret(&secret);
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("无法在 {bind} 上启动 relay（端口被占？）"))?;
        let router = leveler_relay::build_router(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        config.relay_url = format!("http://{address}:18443");
        home.save_config(&config)?;

        enroll(&config, &key, &secret).await?;
        let agent_key = home.load_key()?.context("找不到运行时密钥")?;
        start_agent(runtime, repo_root, &home, &config, agent_key)?;
        session.lock().unwrap().started = true;
    } else if !already_started {
        // Someone else's relay: it must already know this machine, because
        // enrolling needs an operator secret this command does not have.
        home.save_config(&config)?;
        let agent_key = home.load_key()?.context("找不到运行时密钥")?;
        start_agent(runtime, repo_root, &home, &config, agent_key)?;
        session.lock().unwrap().started = true;
    }

    let http = reqwest::Client::new();
    let response = http
        .post(format!("{}/v1/pair/begin", config.relay_url))
        .header(
            RUNTIME_AUTH_HEADER,
            auth_header(&key, runtime_action::PAIR_BEGIN, &config.runtime_id),
        )
        .json(&serde_json::json!({"runtime_id": config.runtime_id}))
        .send()
        .await
        .with_context(|| format!("连接 relay {} 失败", config.relay_url))?;
    if !response.status().is_success() {
        bail!("relay 拒绝了配对请求（HTTP {}）", response.status());
    }
    let body: serde_json::Value = response.json().await?;
    let secret = body["pairing_secret"]
        .as_str()
        .context("relay 没有返回 pairing_secret")?;

    let payload = PairingQrPayload {
        runtime_id: config.runtime_id.clone(),
        runtime_pubkey: key.verifying_key().to_base64url(),
        relay_url: config.relay_url.clone(),
        pairing_secret: secret.to_string(),
    };
    let payload_text = serde_json::to_string(&payload)?;

    Ok(RemoteOutcome::Invited(RemoteInvite {
        qr: qr_rows(&payload_text)?,
        payload: payload_text,
        host_fingerprint: key.verifying_key().fingerprint_display(),
        relay_url: config.relay_url.clone(),
    }))
}

/// Who is waiting, if anyone.
async fn pending(
    home: &RemoteHome,
    session: &Arc<Mutex<RemoteSession>>,
) -> anyhow::Result<RemoteOutcome> {
    let (config, key) = require(home)?;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/pair/pending", config.relay_url))
        .query(&[("runtime_id", config.runtime_id.as_str())])
        .header(
            RUNTIME_AUTH_HEADER,
            auth_header(&key, runtime_action::PAIR_PENDING, &config.runtime_id),
        )
        .send()
        .await?;
    if !response.status().is_success() {
        return Ok(RemoteOutcome::Waiting(None));
    }
    let waiting: Option<leveler_remote_protocol::pairing::PairingPending> = response.json().await?;
    let Some(waiting) = waiting else {
        return Ok(RemoteOutcome::Waiting(None));
    };
    let device_key = VerifyingKey::from_base64url(&waiting.device_pubkey)
        .map_err(|_| anyhow::anyhow!("relay 给的设备公钥无法解析"))?;
    session.lock().unwrap().pairing_id = Some(waiting.pairing_id.clone());

    Ok(RemoteOutcome::Waiting(Some(PairingRequest {
        device_name: waiting.device_name,
        platform: waiting.platform,
        fingerprint: device_key.fingerprint_display(),
    })))
}

/// Accept or reject, writing the key locally *before* telling the relay.
async fn decide(
    home: &RemoteHome,
    session: &Arc<Mutex<RemoteSession>>,
    accept: bool,
) -> anyhow::Result<RemoteOutcome> {
    let (config, key) = require(home)?;
    let http = reqwest::Client::new();

    let response = http
        .get(format!("{}/v1/pair/pending", config.relay_url))
        .query(&[("runtime_id", config.runtime_id.as_str())])
        .header(
            RUNTIME_AUTH_HEADER,
            auth_header(&key, runtime_action::PAIR_PENDING, &config.runtime_id),
        )
        .send()
        .await?;
    let waiting: Option<leveler_remote_protocol::pairing::PairingPending> = response.json().await?;
    let Some(waiting) = waiting else {
        return Ok(RemoteOutcome::Waiting(None));
    };

    if accept {
        let device_key = VerifyingKey::from_base64url(&waiting.device_pubkey)
            .map_err(|_| anyhow::anyhow!("relay 给的设备公钥无法解析"))?;
        // Local record first: a crash between the two must not leave the relay
        // believing a device is paired that this host cannot verify.
        let mut devices = TrustedDevices::load(home.devices_path())?;
        devices.accept(
            &waiting.device_id,
            &device_key,
            &waiting.device_name,
            waiting.scope,
            &now_stamp(),
        )?;
    }

    let confirmed = http
        .post(format!("{}/v1/pair/confirm", config.relay_url))
        .header(
            RUNTIME_AUTH_HEADER,
            auth_header(&key, runtime_action::PAIR_CONFIRM, &config.runtime_id),
        )
        .json(&serde_json::json!({
            "runtime_id": config.runtime_id,
            "pairing_id": waiting.pairing_id,
            "decision": if accept { "accept" } else { "reject" },
        }))
        .send()
        .await?;
    if !confirmed.status().is_success() {
        bail!("relay 拒绝了确认（HTTP {}）", confirmed.status());
    }
    session.lock().unwrap().pairing_id = None;

    Ok(if accept {
        RemoteOutcome::Paired {
            device_name: waiting.device_name,
        }
    } else {
        RemoteOutcome::Rejected
    })
}

fn require(
    home: &RemoteHome,
) -> anyhow::Result<(RemoteConfig, leveler_remote_protocol::SigningKey)> {
    let config = home
        .load_config()?
        .context("本机还没有远程配置，先运行 /remote")?;
    let key = home.load_key()?.context("找不到运行时密钥")?;
    Ok((config, key))
}

async fn enroll(
    config: &RemoteConfig,
    key: &leveler_remote_protocol::SigningKey,
    secret: &str,
) -> anyhow::Result<()> {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/runtimes/enroll", config.relay_url))
        .bearer_auth(secret)
        .header(
            RUNTIME_AUTH_HEADER,
            auth_header(key, runtime_action::ENROLL, &config.runtime_id),
        )
        .json(&serde_json::json!({
            "runtime_id": config.runtime_id,
            "runtime_pubkey": key.verifying_key().to_base64url(),
        }))
        .send()
        .await
        .context("无法向本机 relay 注册")?;
    if !response.status().is_success() {
        bail!("注册失败（HTTP {}）", response.status());
    }
    Ok(())
}

/// Serve paired devices over the runtime this TUI already owns.
fn start_agent(
    runtime: Arc<dyn LocalRuntimeService>,
    repo_root: std::path::PathBuf,
    home: &RemoteHome,
    config: &RemoteConfig,
    key: leveler_remote_protocol::SigningKey,
) -> anyhow::Result<()> {
    let devices = TrustedDevices::load(home.devices_path())?;
    let project_id = leveler_project::Layout::resolve(repo_root.clone(), None)
        .socket_path()
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let display = repo_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "项目".to_string());

    let audit = Arc::new(AuditLog::new(home.dir().join("audit")));
    let bridge = Arc::new(
        AgentBridge::new(
            Arc::new(SingleProject::new(project_id, display, runtime)),
            devices,
            config.runtime_id.clone(),
            key,
            config.allow_full_access,
        )
        .with_audit(audit),
    );

    let ws_url = config.relay_ws_url();
    let runtime_id = config.runtime_id.clone();
    let display_name = config.display_name.clone();
    let timeout = config.approval_timeout();
    tokio::spawn(async move {
        // Reconnect for as long as the TUI lives: a relay restart should not
        // require the user to notice and retype anything.
        let mut backoff = std::time::Duration::from_secs(1);
        loop {
            let started = std::time::Instant::now();
            let _ = run_tunnel(
                &ws_url,
                &runtime_id,
                &display_name,
                bridge.clone(),
                timeout,
                now_stamp,
            )
            .await;
            if started.elapsed() >= std::time::Duration::from_secs(60) {
                backoff = std::time::Duration::from_secs(1);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
        }
    });
    Ok(())
}

fn default_display_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "开发机".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The QR must actually decode back to what a phone needs, and fit on a
    /// terminal. A code that is right but 90 rows tall is a code nobody scans.
    #[test]
    fn the_invite_qr_encodes_the_payload_and_fits_a_terminal() {
        let payload = serde_json::to_string(&PairingQrPayload {
            runtime_id: "rt_a3dd6a43c99c7f46".to_string(),
            runtime_pubkey: "HKnYa5ASZp5Z-WdmjNJAQ5mZW7-XTABC0l7NBVE7-GI".to_string(),
            relay_url: "http://192.168.1.23:18443".to_string(),
            pairing_secret: "3y9mp9_qB53NfJ7lL2biQ8ztTyTAFtUqEE69D_k6ldo".to_string(),
        })
        .unwrap();

        let rows = qr_rows(&payload).expect("a real payload encodes");
        assert!(!rows.is_empty());
        // Half blocks: two module rows per line, plus the quiet zone.
        assert!(
            rows.len() <= 40,
            "{} rows will not fit most terminals",
            rows.len()
        );
        let width = rows[0].chars().count();
        assert!(rows.iter().all(|row| row.chars().count() == width));
        assert!(
            width <= 80,
            "{width} columns is wider than a normal terminal"
        );
    }

    /// Everything a phone needs must survive the round trip; a QR missing the
    /// runtime key would pair a device against a machine it cannot verify.
    #[test]
    fn the_payload_carries_what_the_phone_anchors() {
        let payload = PairingQrPayload {
            runtime_id: "rt_1".to_string(),
            runtime_pubkey: "key".to_string(),
            relay_url: "http://192.168.1.23:18443".to_string(),
            pairing_secret: "secret".to_string(),
        };
        let text = serde_json::to_string(&payload).unwrap();
        let back: PairingQrPayload = serde_json::from_str(&text).unwrap();
        assert_eq!(back, payload);
        assert!(text.contains("runtime_pubkey"));
    }
}
