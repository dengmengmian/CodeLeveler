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

use leveler_remote_agent::TrustedDevices;
use leveler_remote_protocol::VerifyingKey;

use crate::cli::RemoteCommand;

pub(crate) fn cmd_remote(cmd: RemoteCommand) -> anyhow::Result<std::process::ExitCode> {
    match cmd {
        RemoteCommand::Status => status(),
        RemoteCommand::Devices => devices(),
        RemoteCommand::Revoke { device_id } => revoke(&device_id),
    }
}

/// Where the remote agent keeps its state. Sibling of the rest of the leveler
/// home so a user who backs that up gets the pairing record too.
fn remote_dir() -> PathBuf {
    leveler_core::leveler_home_dir_from(|k| std::env::var_os(k))
        .unwrap_or_else(|| PathBuf::from(".leveler"))
        .join("remote")
}

fn devices_path() -> PathBuf {
    remote_dir().join("devices.json")
}

fn status() -> anyhow::Result<std::process::ExitCode> {
    let path = devices_path();
    let store = TrustedDevices::load(&path)?;
    let active = store.devices().iter().filter(|d| d.is_active()).count();
    let revoked = store.devices().len() - active;

    println!("远程控制状态");
    println!("  设备记录：{}", path.display());
    println!("  已配对设备：{active} 台（另有 {revoked} 台已撤销）");
    println!();
    println!("  注意：本机尚未内置 relay 连接，配对流程需要 relay 就绪后才能完成。");
    println!(
        "  当前可用：`leveler remote devices` 查看已信任设备，`leveler remote revoke <id>` 撤销。"
    );
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

fn revoke(device_id: &str) -> anyhow::Result<std::process::ExitCode> {
    let path = devices_path();
    let mut store = TrustedDevices::load(&path)?;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    if store.revoke(device_id, &now)? {
        println!("已撤销设备 {device_id}。");
        println!("该设备的后续帧将被拒绝（下一帧即生效，无需等待重连）。");
        Ok(std::process::ExitCode::SUCCESS)
    } else {
        eprintln!("找不到设备 {device_id}。用 `leveler remote devices` 查看已配对设备。");
        Ok(std::process::ExitCode::FAILURE)
    }
}
