//! Windows probes and the static PowerShell plans.
//!
//! Discovery reads registry and file facts only. It never creates a COM object:
//! instantiating `Outlook.Application` can start Outlook and can prompt, which
//! is a side effect discovery deliberately avoids.

use std::path::Path;
use std::time::Duration;

use winreg::RegKey;
use winreg::enums::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER};

use crate::classify::{WindowsMailProbe, windows_mail};
use crate::discover::{DiscoveryReport, SystemToolReport};
use crate::model::{ActionId, ActionPlan, NativeError, ProviderId};
use crate::protocol::{Payload, write_private};

const NEW_OUTLOOK_PACKAGES: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";
const MAILTO_USER_CHOICE: &str =
    r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\mailto\UserChoice";

const ACTION_SCRIPT: &str = r#"param(
    [Parameter(Mandatory=$true)][ValidateSet('draft','open_ui','submit')][string]$Mode,
    [Parameter(Mandatory=$true)][string]$PayloadPath
)
$ErrorActionPreference = 'Stop'
$payload = Get-Content -LiteralPath $PayloadPath -Raw -Encoding UTF8 | ConvertFrom-Json
$to = [string]$payload.to
$subject = [string]$payload.subject
$body = [string]$payload.body
try {
    $outlook = New-Object -ComObject Outlook.Application
} catch {
    Write-Output 'ERROR COM_UNAVAILABLE'
    Write-Error $_.Exception.Message
    exit 3
}
try {
    $mail = $outlook.CreateItem(0)
    $mail.To = $to
    $mail.Subject = $subject
    $mail.Body = $body
    switch ($Mode) {
        'draft' { $mail.Save(); Write-Output 'DRAFT_CREATED' }
        'open_ui' { $mail.Display(); Write-Output 'COMPOSE_OPENED' }
        'submit' { $mail.Send(); Write-Output 'SEND_SUBMITTED' }
    }
} catch {
    Write-Output 'ERROR ACTION_FAILED'
    Write-Error $_.Exception.Message
    exit 4
}
"#;

const SENT_CHECK_SCRIPT: &str = r#"param(
    [Parameter(Mandatory=$true)][string]$PayloadPath
)
$ErrorActionPreference = 'Stop'
$payload = Get-Content -LiteralPath $PayloadPath -Raw -Encoding UTF8 | ConvertFrom-Json
$to = [string]$payload.to
$subject = [string]$payload.subject
try {
    $outlook = New-Object -ComObject Outlook.Application
} catch {
    Write-Output 'ERROR COM_UNAVAILABLE'
    Write-Error $_.Exception.Message
    exit 3
}
try {
    $namespace = $outlook.GetNamespace('MAPI')
    $sentBox = $namespace.GetDefaultFolder(5)
    $items = $sentBox.Items
    $items.Sort('[SentOn]', $true)
    $total = $items.Count
    $limit = [Math]::Min(200, $total)
    $found = 0
    for ($i = 1; $i -le $limit; $i++) {
        try {
            $item = $items.Item($i)
            if ([string]$item.Subject -ceq $subject) {
                $recipients = $item.Recipients
                for ($r = 1; $r -le $recipients.Count; $r++) {
                    if ([string]$recipients.Item($r).Address -ieq $to) {
                        $found = $found + 1
                        break
                    }
                }
            }
        } catch {
            # A message that cannot be read is not a match; the scan continues.
        }
    }
    Write-Output "SENT_FOUND $found SCANNED $limit TOTAL $total"
} catch {
    Write-Output 'ERROR ACTION_FAILED'
    Write-Error $_.Exception.Message
    exit 4
}
"#;

pub(super) fn discover() -> DiscoveryReport {
    let mut probe_errors = Vec::new();

    let classic_progid = RegKey::predef(HKEY_CLASSES_ROOT)
        .open_subkey("Outlook.Application")
        .is_ok();
    let classic_profile = classic_profile_exists(&mut probe_errors);
    let new_outlook = new_outlook_installed(&mut probe_errors);
    let default_handler = default_mail_handler(&mut probe_errors);
    let powershell = powershell_path();

    let probe = WindowsMailProbe {
        classic_progid,
        classic_profile,
        new_outlook,
        default_handler: default_handler.clone(),
        powershell: powershell.clone(),
        probe_errors,
    };
    let capabilities = windows_mail(&probe);

    let mut notes = vec![
        "Discovery reads registry and file facts only; it does not create an Outlook COM object."
            .to_string(),
        "New Outlook and classic Outlook are distinct providers with distinct automation support."
            .to_string(),
    ];
    if new_outlook && classic_progid {
        notes.push(
            "Both new Outlook and a classic Outlook ProgID are present; the default mail handler \
             says which one the OS would open, not which one can be automated."
                .to_string(),
        );
    }

    DiscoveryReport {
        platform: "windows",
        supported: true,
        default_mail_handler: default_handler,
        system_tools: vec![SystemToolReport {
            id: "powershell".to_string(),
            installed: powershell.is_some(),
            path: powershell.clone(),
            reason: Some(
                "runs the static Outlook action plans; Windows PowerShell 5.1 is required"
                    .to_string(),
            ),
        }],
        capabilities,
        notes,
    }
}

fn classic_profile_exists(errors: &mut Vec<String>) -> bool {
    let office = match RegKey::predef(HKEY_CURRENT_USER).open_subkey(r"Software\Microsoft\Office") {
        Ok(key) => key,
        Err(_) => return false,
    };
    let versions = match office.enum_keys().collect::<Result<Vec<_>, _>>() {
        Ok(versions) => versions,
        Err(error) => {
            errors.push(format!("could not enumerate Office versions: {error}"));
            return false;
        }
    };
    for version in versions {
        let profiles = format!(r"{version}\Outlook\Profiles");
        if let Ok(key) = office.open_subkey(&profiles) {
            if key.enum_keys().next().is_some() {
                return true;
            }
        }
    }
    false
}

fn new_outlook_installed(errors: &mut Vec<String>) -> bool {
    let packages = match RegKey::predef(HKEY_CURRENT_USER).open_subkey(NEW_OUTLOOK_PACKAGES) {
        Ok(key) => key,
        Err(_) => return false,
    };
    let names = match packages.enum_keys().collect::<Result<Vec<_>, _>>() {
        Ok(names) => names,
        Err(error) => {
            errors.push(format!("could not enumerate AppX packages: {error}"));
            return false;
        }
    };
    names
        .iter()
        .any(|name| name.starts_with("Microsoft.OutlookForWindows_"))
}

fn default_mail_handler(errors: &mut Vec<String>) -> Option<String> {
    let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey(MAILTO_USER_CHOICE) {
        Ok(key) => key,
        Err(_) => return None,
    };
    match key.get_value::<String, _>("ProgId") {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(format!("could not read the mailto handler: {error}"));
            None
        }
    }
}

fn powershell_path() -> Option<String> {
    let root = std::env::var_os("SystemRoot")?;
    let path = Path::new(&root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    path.is_file().then(|| path.to_string_lossy().to_string())
}

pub(super) fn plan(action: ActionId, payload: Payload) -> Result<ActionPlan, NativeError> {
    let powershell = powershell_path().ok_or_else(|| {
        NativeError::Unsupported(
            action.as_str(),
            "Windows PowerShell 5.1 is not available, so the Outlook plan cannot run".to_string(),
        )
    })?;
    let directory = tempfile::Builder::new()
        .prefix("leveler-native-")
        .tempdir()
        .map_err(|error| NativeError::Prepare {
            action: action.as_str(),
            detail: format!("could not create a private payload directory: {error}"),
        })?;
    let write = |name: &str, bytes: &[u8]| {
        write_private(&directory.path().join(name), bytes).map_err(|error| NativeError::Prepare {
            action: action.as_str(),
            detail: format!("could not write {name}: {error}"),
        })
    };

    let (script_name, script, mode, payload_name, payload_bytes) = match (&payload, action) {
        (Payload::Message { to, subject, body }, action) => {
            let payload = serde_json::json!({
                "to": to,
                "subject": subject,
                "body": body,
            });
            let bytes = serde_json::to_vec(&payload).map_err(|error| NativeError::Prepare {
                action: action.as_str(),
                detail: format!("could not encode the payload: {error}"),
            })?;
            let mode = match action {
                ActionId::MailDraftCreate => "draft",
                ActionId::MailSendOpenUi => "open_ui",
                ActionId::MailSendSubmit => "submit",
                ActionId::MailSentCheck => unreachable!("a check never carries a message"),
            };
            (
                "outlook_action.ps1",
                ACTION_SCRIPT,
                Some(mode),
                "payload.json",
                bytes,
            )
        }
        (Payload::Check { to, subject }, _) => {
            let payload = serde_json::json!({ "to": to, "subject": subject });
            let bytes = serde_json::to_vec(&payload).map_err(|error| NativeError::Prepare {
                action: action.as_str(),
                detail: format!("could not encode the payload: {error}"),
            })?;
            (
                "outlook_sent_check.ps1",
                SENT_CHECK_SCRIPT,
                None,
                "payload.json",
                bytes,
            )
        }
    };

    write(script_name, script.as_bytes())?;
    write(payload_name, &payload_bytes)?;
    let script_path = directory.path().join(script_name);
    let payload_path = directory.path().join(payload_name);

    let mut args = vec![
        "-NoProfile".to_string(),
        "-NonInteractive".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-File".to_string(),
        script_path.to_string_lossy().to_string(),
    ];
    if let Some(mode) = mode {
        args.push("-Mode".to_string());
        args.push(mode.to_string());
    }
    args.push("-PayloadPath".to_string());
    args.push(payload_path.to_string_lossy().to_string());

    let timeout = match action {
        ActionId::MailDraftCreate | ActionId::MailSendOpenUi => Duration::from_secs(90),
        ActionId::MailSendSubmit => Duration::from_secs(120),
        ActionId::MailSentCheck => Duration::from_secs(240),
    };
    Ok(ActionPlan {
        action,
        provider: ProviderId::WindowsOutlookClassic,
        program: powershell,
        args,
        cwd: directory.path().to_path_buf(),
        timeout,
        _payload: directory,
    })
}
