//! CLI surface for remote APP control.
//!
//! Pairing is deliberately a human step. What the user confirms when accepting
//! a device is **that device's public key**, shown as a fingerprint they can
//! compare against the one on the phone's screen — not a nickname a relay chose.
//! A relay that could substitute the key while the user nodded at a familiar
//! name would defeat pairing entirely while leaving every later signature check
//! looking like it still worked. So acceptance happens here, on the developer's
//! own terminal, and it is what writes the key to `devices.json`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use leveler_remote_agent::{
    AgentBridge, ProjectRouter, RemoteConfig, RemoteHome, TrustedDevices, run_tunnel,
    runtime_id_for,
};
use leveler_remote_protocol::VerifyingKey;
use leveler_remote_protocol::auth::{RUNTIME_AUTH_HEADER, RuntimeAssertion, runtime_action};
use leveler_remote_protocol::pairing::{PairingQrPayload, PairingScope};

use crate::cli::RemoteCommand;

pub(crate) async fn cmd_remote(cmd: RemoteCommand) -> anyhow::Result<std::process::ExitCode> {
    match cmd {
        RemoteCommand::Enable { relay_url, name } => enable(&relay_url, name),
        RemoteCommand::Enroll => enroll().await,
        RemoteCommand::Pair { scope } => pair(scope.as_deref()).await,
        RemoteCommand::Pending => pending().await,
        RemoteCommand::Confirm { reject, yes } => confirm(reject, yes).await,
        RemoteCommand::Projects => projects().await,
        RemoteCommand::Agent => agent().await,
        RemoteCommand::Status => status(),
        RemoteCommand::Devices => devices(),
        RemoteCommand::Revoke { device_id } => revoke(&device_id).await,
    }
}

fn leveler_home() -> leveler_core::LevelerHome {
    leveler_core::LevelerHome::resolve(leveler_core::environment())
}

/// Where the remote agent keeps its state (`state/remote/` under the home).
fn remote_dir() -> PathBuf {
    leveler_home().remote_state_dir()
}

fn home() -> RemoteHome {
    RemoteHome::new(remote_dir())
}

fn devices_path() -> PathBuf {
    remote_dir().join("devices.json")
}

/// The multi-project registry the browser UI writes. Read-only from here: what
/// is "open" is decided by opening it, not by a phone asking.
fn registry_path() -> PathBuf {
    leveler_home().web_projects_registry()
}

/// Config and key together, or a message naming the command that creates them.
fn require_enabled() -> anyhow::Result<(
    RemoteConfig,
    leveler_remote_protocol::SigningKey,
    RemoteHome,
)> {
    let home = home();
    let Some(config) = home.load_config()? else {
        bail!("本机尚未启用远程控制。先运行：leveler remote enable --relay-url <URL>");
    };
    let Some(key) = home.load_key()? else {
        bail!(
            "配置存在但找不到运行时密钥（{}）。删除 config.toml 后重新 enable，并重新配对所有设备。",
            home.key_path().display()
        );
    };
    Ok((config, key, home))
}

/// A fresh nonce for a runtime assertion; the relay spends each exactly once.
fn nonce() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("system RNG is available");
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn now_stamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Sign one control-plane request as this machine.
fn auth_header(
    key: &leveler_remote_protocol::SigningKey,
    action: &str,
    runtime_id: &str,
) -> String {
    RuntimeAssertion::header_value(key, action, runtime_id, &now_stamp(), &nonce())
}

/// Turn a relay error body into something a user can act on.
async fn relay_error(what: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    let code = body["code"].as_str().unwrap_or("unknown");
    anyhow::anyhow!("{what} 失败（HTTP {status}，code={code}）")
}

fn enable(relay_url: &str, name: Option<String>) -> anyhow::Result<std::process::ExitCode> {
    let home = home();
    // Keep an existing identity: regenerating would leave every paired phone
    // verifying against a key this host no longer has.
    let key = match home.load_key()? {
        Some(key) => {
            println!("已存在运行时密钥，保留不变（重新生成会让所有已配对设备失效）。");
            key
        }
        None => home.create_key()?,
    };

    let public = key.verifying_key();
    let runtime_id = runtime_id_for(&public);
    let display_name = name.unwrap_or_else(default_display_name);

    let mut config = home
        .load_config()?
        .unwrap_or_else(|| RemoteConfig::new(relay_url, &display_name, runtime_id.clone()));
    config.relay_url = relay_url.trim_end_matches('/').to_string();
    config.display_name = display_name;
    config.runtime_id = runtime_id.clone();
    home.save_config(&config)?;

    println!("远程控制已启用。");
    println!("  机器 id：{runtime_id}");
    println!("  公钥指纹：{}", public.fingerprint_display());
    println!("  relay：{}", config.relay_url);
    println!("  配置：{}", home.config_path().display());
    println!();
    println!("下一步：");
    println!("  1. leveler remote enroll        # 用 relay 运营者密钥注册本机公钥");
    println!("  2. leveler remote agent         # 常驻进程，出站连接 relay");
    println!("  3. leveler remote pair          # 生成配对载荷给手机");
    Ok(std::process::ExitCode::SUCCESS)
}

fn default_display_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
        })
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "开发机".to_string())
}

/// Read the operator's enrollment secret without putting it in argv.
fn read_enrollment_secret() -> anyhow::Result<String> {
    if let Ok(secret) = std::env::var("LEVELER_RELAY_ENROLLMENT_SECRET")
        && !secret.is_empty()
    {
        return Ok(secret);
    }
    // No hidden input: that needs a terminal-handling dependency this CLI does
    // not have, and pretending otherwise would be worse than saying so.
    eprint!(
        "relay 运营者密钥（输入会显示在屏幕上；也可用 LEVELER_RELAY_ENROLLMENT_SECRET 传入）："
    );
    use std::io::Write as _;
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("读取 relay 密钥失败")?;
    let secret = line.trim().to_string();
    if secret.is_empty() {
        bail!("没有提供 relay 运营者密钥。也可用环境变量 LEVELER_RELAY_ENROLLMENT_SECRET 传入。");
    }
    Ok(secret)
}

async fn enroll() -> anyhow::Result<std::process::ExitCode> {
    let (config, key, _home) = require_enabled()?;
    let secret = read_enrollment_secret()?;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/runtimes/enroll", config.relay_url))
        .bearer_auth(secret)
        .header(
            RUNTIME_AUTH_HEADER,
            auth_header(&key, runtime_action::ENROLL, &config.runtime_id),
        )
        .json(&serde_json::json!({
            "runtime_id": config.runtime_id,
            "runtime_pubkey": key.verifying_key().to_base64url(),
        }))
        .send()
        .await
        .with_context(|| format!("连接 relay {} 失败", config.relay_url))?;

    if !response.status().is_success() {
        return Err(relay_error("注册", response).await);
    }
    println!("已在 relay 上注册本机：{}", config.runtime_id);
    println!("relay 从此只认这把密钥；换密钥需要在 relay 上先解除绑定。");
    Ok(std::process::ExitCode::SUCCESS)
}

async fn pair(scope: Option<&str>) -> anyhow::Result<std::process::ExitCode> {
    let (config, key, _home) = require_enabled()?;
    let scope = match scope {
        None => config.default_pair_scope,
        Some("interactive") => PairingScope::Interactive,
        Some("observe") => PairingScope::Observe,
        Some(other) => bail!("未知的 scope `{other}`，可选 interactive 或 observe"),
    };

    let response = reqwest::Client::new()
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
        return Err(relay_error("发起配对", response).await);
    }
    let body: serde_json::Value = response.json().await?;
    let secret = body["pairing_secret"]
        .as_str()
        .context("relay 没有返回 pairing_secret")?;
    let ttl = body["ttl_secs"].as_i64().unwrap_or(300);

    // The payload carries the runtime's public key, so the phone anchors this
    // machine's identity here rather than trusting the relay's later claim.
    let payload = PairingQrPayload {
        runtime_id: config.runtime_id.clone(),
        runtime_pubkey: key.verifying_key().to_base64url(),
        relay_url: config.relay_url.clone(),
        pairing_secret: secret.to_string(),
    };

    println!("配对载荷（{} 分钟内有效，仅可用一次）：", ttl / 60);
    println!();
    println!("{}", serde_json::to_string(&payload)?);
    println!();
    println!("在手机 APP 里粘贴上面这行。（二维码渲染尚未内置，粘贴是设计里等价的路径。）");
    println!("权限：{scope:?}");
    println!("手机提交后，在本机运行：leveler remote confirm");
    Ok(std::process::ExitCode::SUCCESS)
}

/// Who is waiting to pair, if anyone. Exit code 1 when nobody is, so a script
/// can branch on it.
async fn pending() -> anyhow::Result<std::process::ExitCode> {
    let Some((pending, _config, _key)) = pending_pairing().await? else {
        println!("当前没有等待确认的配对。");
        return Ok(std::process::ExitCode::FAILURE);
    };
    let device_key = VerifyingKey::from_base64url(&pending.device_pubkey)
        .map_err(|_| anyhow::anyhow!("relay 给的设备公钥无法解析"))?;
    println!("等待确认：{} ({})", pending.device_name, pending.device_id);
    println!("  平台：{}", pending.platform);
    println!("  权限：{:?}", pending.scope);
    println!("  指纹：{}", device_key.fingerprint_display());
    println!();
    println!("接受：leveler remote confirm      拒绝：leveler remote confirm --reject");
    Ok(std::process::ExitCode::SUCCESS)
}

/// Ask the relay what is waiting for this host, with the config and key the
/// caller will need next.
async fn pending_pairing() -> anyhow::Result<
    Option<(
        leveler_remote_protocol::pairing::PairingPending,
        RemoteConfig,
        leveler_remote_protocol::SigningKey,
    )>,
> {
    let (config, key, _home) = require_enabled()?;
    let response = reqwest::Client::new()
        .get(format!("{}/v1/pair/pending", config.relay_url))
        .query(&[("runtime_id", config.runtime_id.as_str())])
        .header(
            RUNTIME_AUTH_HEADER,
            auth_header(&key, runtime_action::PAIR_PENDING, &config.runtime_id),
        )
        .send()
        .await
        .with_context(|| format!("连接 relay {} 失败", config.relay_url))?;
    if !response.status().is_success() {
        return Err(relay_error("查询待确认配对", response).await);
    }
    let pending: Option<leveler_remote_protocol::pairing::PairingPending> = response.json().await?;
    Ok(pending.map(|pending| (pending, config, key)))
}

async fn confirm(reject: bool, yes: bool) -> anyhow::Result<std::process::ExitCode> {
    let home = home();
    let http = reqwest::Client::new();

    let Some((pending, config, key)) = pending_pairing().await? else {
        println!("当前没有等待确认的配对。先运行 `leveler remote pair`，再在手机上提交。");
        return Ok(std::process::ExitCode::SUCCESS);
    };

    // The fingerprint is derived here from the key that will actually be
    // trusted — not copied from a label the relay chose.
    let device_key = VerifyingKey::from_base64url(&pending.device_pubkey)
        .map_err(|_| anyhow::anyhow!("relay 给的设备公钥无法解析，拒绝配对"))?;

    println!("等待确认的设备：");
    println!("  名称：{}", pending.device_name);
    println!("  平台：{}", pending.platform);
    println!("  权限：{:?}", pending.scope);
    println!("  指纹：{}", device_key.fingerprint_display());
    println!();
    println!("请与手机上显示的指纹逐字比对——你确认的是这把密钥，不是设备名字。");

    if reject {
        return finish_pairing(&http, &config, &key, &pending.pairing_id, false).await;
    }
    if !yes {
        use std::io::Write as _;
        print!("接受这台设备？[y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!("已拒绝。");
            return finish_pairing(&http, &config, &key, &pending.pairing_id, false).await;
        }
    }

    // Persist before telling the relay. A crash between the two must not leave
    // the relay believing a device is paired that this host cannot verify.
    let mut store = TrustedDevices::load(home.devices_path())?;
    store.accept(
        &pending.device_id,
        &device_key,
        &pending.device_name,
        pending.scope,
        &now_stamp(),
    )?;
    println!("已写入本机信任记录：{}", home.devices_path().display());

    finish_pairing(&http, &config, &key, &pending.pairing_id, true).await
}

async fn finish_pairing(
    http: &reqwest::Client,
    config: &RemoteConfig,
    key: &leveler_remote_protocol::SigningKey,
    pairing_id: &str,
    accept: bool,
) -> anyhow::Result<std::process::ExitCode> {
    let response = http
        .post(format!("{}/v1/pair/confirm", config.relay_url))
        .header(
            RUNTIME_AUTH_HEADER,
            auth_header(key, runtime_action::PAIR_CONFIRM, &config.runtime_id),
        )
        .json(&serde_json::json!({
            "runtime_id": config.runtime_id,
            "pairing_id": pairing_id,
            "decision": if accept { "accept" } else { "reject" },
        }))
        .send()
        .await
        .context("向 relay 提交配对结果失败")?;
    if !response.status().is_success() {
        return Err(relay_error("确认配对", response).await);
    }
    if accept {
        println!("配对完成。手机现在可以取 token 并连接。");
    } else {
        println!("已通知 relay 拒绝该配对。本机未记录任何密钥。");
    }
    Ok(std::process::ExitCode::SUCCESS)
}

async fn projects() -> anyhow::Result<std::process::ExitCode> {
    let router = project_router();
    let listed = leveler_remote_agent::ProjectRoutes::projects(&router).await;
    if listed.is_empty() {
        println!(
            "没有已打开的项目。用 `leveler web` 打开一个仓库，或在该仓库运行 `leveler serve`。"
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }
    println!("手机可见的项目：");
    for project in listed {
        let status = match project.status {
            leveler_session_wire::ProjectStatus::Online => "在线",
            leveler_session_wire::ProjectStatus::Starting => "启动中",
            leveler_session_wire::ProjectStatus::Offline => "离线",
        };
        println!(
            "  {:<20} {:<10} {}",
            project.path_display, status, project.project_id
        );
    }
    println!();
    println!("离线的项目在手机上是灰的：远程 agent 只连接已在运行的 daemon，不会替你启动。");
    Ok(std::process::ExitCode::SUCCESS)
}

/// The router every remote path shares: the browser UI's registry, and the same
/// per-repository socket the daemon already listens on.
fn project_router() -> ProjectRouter {
    ProjectRouter::new(registry_path(), |repo: &std::path::Path| {
        leveler_project::Layout::resolve(repo.to_path_buf(), None).socket_path()
    })
}

async fn agent() -> anyhow::Result<std::process::ExitCode> {
    let (config, key, home) = require_enabled()?;
    let devices = TrustedDevices::load(home.devices_path())?;
    let paired = devices.devices().iter().filter(|d| d.is_active()).count();

    let audit = Arc::new(leveler_remote_agent::AuditLog::new(
        home.dir().join("audit"),
    ));
    let bridge = Arc::new(
        AgentBridge::new(
            Arc::new(project_router()),
            devices,
            config.runtime_id.clone(),
            key,
            config.allow_full_access,
        )
        .with_audit(audit.clone()),
    );

    println!("远程 agent 启动：{}", config.runtime_id);
    println!("  relay：{}", config.relay_url);
    println!("  已配对设备：{paired} 台");
    println!(
        "  审批超时：{} 秒（仅在无本机 UI 时生效）",
        config.approval_timeout_secs
    );
    println!(
        "  审计日志：{}（按天轮转，保留 {} 天，不记录消息内容）",
        audit.dir().display(),
        leveler_remote_agent::DEFAULT_RETENTION_DAYS
    );
    println!("按 Ctrl-C 停止。");

    let ws_url = config.relay_ws_url();
    // Reconnect with backoff. The relay going away is ordinary — a restart, a
    // network change — and a host that gave up on the first failure would need
    // a human to notice and restart it.
    let mut backoff = std::time::Duration::from_secs(1);
    loop {
        let started = std::time::Instant::now();
        match run_tunnel(
            &ws_url,
            &config.runtime_id,
            &config.display_name,
            bridge.clone(),
            config.approval_timeout(),
            now_stamp,
        )
        .await
        {
            Ok(()) => println!("与 relay 的连接已关闭，准备重连。"),
            Err(error) => eprintln!("与 relay 的连接失败：{error}"),
        }
        // A connection that lasted a while is not a failing one; start over
        // from the short delay rather than punishing it for eventually ending.
        if started.elapsed() >= std::time::Duration::from_secs(60) {
            backoff = std::time::Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(std::time::Duration::from_secs(30));
    }
}

fn status() -> anyhow::Result<std::process::ExitCode> {
    let home = home();
    let path = devices_path();
    let store = TrustedDevices::load(&path)?;
    let active = store.devices().iter().filter(|d| d.is_active()).count();
    let revoked = store.devices().len() - active;

    println!("远程控制状态");
    match home.load_config()? {
        Some(config) => {
            println!("  状态：已启用");
            println!("  机器 id：{}", config.runtime_id);
            match home.load_key()? {
                Some(key) => println!("  公钥指纹：{}", key.verifying_key().fingerprint_display()),
                None => println!("  ⚠️  找不到运行时密钥，配置无法使用"),
            }
            println!("  relay：{}", config.relay_url);
            println!("  审批超时：{} 秒", config.approval_timeout_secs);
            println!(
                "  远程 full access：{}",
                if config.allow_full_access {
                    "允许（本机已显式开启）"
                } else {
                    "拒绝"
                }
            );
        }
        None => {
            println!("  状态：未启用");
            println!("  启用：leveler remote enable --relay-url <URL>");
        }
    }
    println!("  设备记录：{}", path.display());
    println!("  已配对设备：{active} 台（另有 {revoked} 台已撤销）");
    Ok(std::process::ExitCode::SUCCESS)
}

fn devices() -> anyhow::Result<std::process::ExitCode> {
    let store = TrustedDevices::load(devices_path())?;
    if store.devices().is_empty() {
        println!("尚无已配对设备。");
        return Ok(std::process::ExitCode::SUCCESS);
    }

    println!("已配对设备：");
    for device in store.devices() {
        // Derive the fingerprint from the stored key rather than trusting the
        // cached string beside it. If someone swapped the key in this file, the
        // two disagree — and what the user confirmed was the key.
        let derived = VerifyingKey::from_base64url(&device.device_pubkey_b64).ok();
        let shown = derived
            .as_ref()
            .map(|key| key.fingerprint_display())
            .unwrap_or_else(|| "<密钥无法解析>".to_string());

        let state = match &device.revoked_at {
            Some(at) => format!("已撤销 {at}"),
            None => "有效".to_string(),
        };
        println!();
        println!("  {} ({})", device.name, device.device_id);
        println!("    指纹：{shown}");
        println!("    权限：{:?}", device.scope);
        println!("    配对于：{}", device.paired_at);
        println!("    状态：{state}");

        let matches_record = derived
            .as_ref()
            .is_some_and(|key| key.fingerprint() == device.fingerprint);
        if !matches_record {
            println!(
                "    ⚠️  公钥与记录的指纹对不上——devices.json 可能被改动过，请撤销并重新配对。"
            );
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}

/// Withdraw trust in a device, locally and on the relay.
///
/// Local first, and unconditionally: that file is what the agent consults on
/// every frame, so writing it is what actually stops the device. The relay call
/// is what ends the *stream* it is already holding and invalidates its tokens —
/// without it a revoked phone keeps a live socket and collects a refusal per
/// frame instead of a clean close. A relay that cannot be reached therefore
/// downgrades the result rather than failing it: the device is stopped either
/// way, and saying "revoke failed" would invite the user to try again as if it
/// had not worked.
async fn revoke(device_id: &str) -> anyhow::Result<std::process::ExitCode> {
    let path = devices_path();
    let mut store = TrustedDevices::load(&path)?;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    if !store.revoke(device_id, &now)? {
        eprintln!("找不到设备 {device_id}。用 `leveler remote devices` 查看已配对设备。");
        return Ok(std::process::ExitCode::FAILURE);
    }
    println!("已撤销设备 {device_id}。");
    println!("该设备的后续帧将被拒绝（下一帧即生效，无需等待重连）。");

    let (config, key, _home) = match require_enabled() {
        Ok(parts) => parts,
        Err(_) => return Ok(std::process::ExitCode::SUCCESS),
    };
    let response = reqwest::Client::new()
        .delete(format!("{}/v1/devices/{device_id}", config.relay_url))
        .query(&[("runtime_id", config.runtime_id.as_str())])
        .header(
            RUNTIME_AUTH_HEADER,
            auth_header(&key, runtime_action::DEVICE_REVOKE, &config.runtime_id),
        )
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            println!("relay 也已撤销：该设备的 token 立即失效，在途连接已关闭。");
        }
        Ok(response) => {
            println!(
                "本机已撤销，但 relay 没有接受（HTTP {}）。该设备发不出有效指令，\n                 但它与 relay 的连接可能还开着；relay 恢复后可重跑本命令。",
                response.status()
            );
        }
        Err(error) => {
            println!(
                "本机已撤销，但联系不上 relay（{error}）。该设备发不出有效指令，\n                 但它与 relay 的连接可能还开着；网络恢复后可重跑本命令。"
            );
        }
    }
    Ok(std::process::ExitCode::SUCCESS)
}
