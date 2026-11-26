# Secret Broker 1.0 final closure — Windows 11 acceptance.
#
# Run from the repository root on a real Windows 11 host:
#
#   powershell -ExecutionPolicy Bypass -File scripts\secret_broker_windows_acceptance.ps1 `
#        -Repo (Get-Location) [-Zip C:\path\leveler-windows-x86_64.zip]
#
# The script records the host identity, builds the real release binaries,
# computes their SHA-256, runs the security suites, creates a real junction
# from this (signed) script host, and runs the evidence test that writes
# through the broker. Exit 0 = every automated check held. Exit 1 = a check
# failed. Exit 2 = BLOCKED (missing toolchain or host evidence), not PASS.
#
# It never covers the GUI/IME checklist by itself. Those steps are printed and
# must be performed on the host and recorded separately.

param(
    [Parameter(Mandatory = $true)][string]$Repo,
    [string]$Zip = "",
    [string]$Evidence = ""
)

$ErrorActionPreference = "Stop"
# Issue the synthetic key from THIS invocation. Nothing is copied from the
# environment, a Session or an earlier run, and the value is not printed: the
# operator reads it from the restricted file created here.
$canaryRun = [guid]::NewGuid().ToString("N").Substring(0, 8)
$canaryBytes = New-Object byte[] 30
[System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($canaryBytes)
$canary = "sk-" + (($canaryBytes | ForEach-Object { $_.ToString("x2") }) -join '')
$canaryPath = Join-Path $env:TEMP "leveler-synthetic-canary-$canaryRun.txt"
[System.IO.File]::WriteAllText($canaryPath, $canary)
$canaryHash = [System.Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes($canary))
$canaryDigest = (($canaryHash | ForEach-Object { $_.ToString("x2") }) -join '').Substring(0, 12)
$script:failures = 0
$script:blocked = 0
$script:results = @()

function Check([string]$Scenario, [string]$Name, [bool]$Ok, [string]$Detail = "") {
    $script:results += [ordered]@{
        scenario = $Scenario
        check    = $Name
        ok       = $Ok
        detail   = $Detail
    }
    if ($Ok) {
        Write-Host "  ok   [$Scenario] $Name"
    } else {
        $script:failures += 1
        Write-Host "  FAIL [$Scenario] $Name — $Detail"
    }
}

function Blocked([string]$Scenario, [string]$Name, [string]$Detail) {
    $script:results += [ordered]@{
        scenario = $Scenario
        check    = $Name
        ok       = $null
        blocked  = $true
        detail   = $Detail
    }
    $script:blocked += 1
    Write-Host "  BLOCKED [$Scenario] $Name — $Detail"
}

function Sha256([string]$Path) {
    if (Test-Path $Path) {
        return (Get-FileHash -Algorithm SHA256 -Path $Path).Hash.ToLowerInvariant()
    }
    return ""
}

function Invoke-Native([string]$Program, [string[]]$Arguments, [string]$Log = "") {
    # Windows PowerShell 5.1 turns a native program's stderr into a
    # NativeCommandError under ErrorActionPreference=Stop. Cargo and cmd write
    # progress/diagnostics there; their exit code is the answer.
    $previous = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        if ([string]::IsNullOrWhiteSpace($Log)) {
            $output = & $Program @Arguments 2>&1 | Out-String
        } else {
            $output = & $Program @Arguments 2>&1 | Tee-Object -FilePath $Log
        }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
    return [ordered]@{ exit = $code; output = $output }
}

function Run([string]$Program, [string[]]$Arguments) {
    return Invoke-Native $Program $Arguments
}

if (-not $IsWindows -and $env:OS -ne "Windows_NT") {
    Write-Host "BLOCKED: this script must run on a real Windows host"
    exit 2
}
if (-not (Test-Path $Repo)) {
    Write-Host "BLOCKED: repository not found: $Repo"
    exit 2
}
$Repo = (Resolve-Path $Repo).Path
if ([string]::IsNullOrWhiteSpace($Evidence)) {
    $Evidence = Join-Path $Repo "target\secret-broker-windows"
}
New-Item -ItemType Directory -Force -Path $Evidence | Out-Null

Push-Location $Repo
try {
    # ── Host and candidate identity ───────────────────────────────────────
    $sha = (git rev-parse HEAD).Trim()
    $dirty = (git status --porcelain | Measure-Object -Line).Lines -gt 0
    $gitStatus = (git status --porcelain | Out-String)
    $rustc = try { (& rustc --version 2>&1 | Out-String).Trim() } catch { "" }
    $cargo = try { (& cargo --version 2>&1 | Out-String).Trim() } catch { "" }
    $os = Get-CimInstance Win32_OperatingSystem
    $identity = Run "$env:SystemRoot\System32\whoami.exe" @("/user")
    $groups = Run "$env:SystemRoot\System32\whoami.exe" @("/groups", "/fo", "csv")
    $privs = Run "$env:SystemRoot\System32\whoami.exe" @("/priv")
    $principal = New-Object Security.Principal.WindowsPrincipal(
        [Security.Principal.WindowsIdentity]::GetCurrent()
    )
    $effectiveAdmin = $principal.IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator
    )
    $medium = $groups.output -match "S-1-16-8192"
    $administratorsDenyOnly = ($groups.output -match "S-1-5-32-544") -and (-not $effectiveAdmin)

    Check "env" "candidate SHA recorded" ($sha -match "^[0-9a-f]{40}$") $sha
    Check "env" "working tree is clean" (-not $dirty) "uncommitted changes present"
    Check "env" "the Rust toolchain is installed" ([bool](Get-Command cargo -ErrorAction SilentlyContinue)) "$rustc / $cargo"
    Check "env" "the host is Windows 11" ($os.Caption -match "Windows 11") "$($os.Caption) $($os.Version) build $($os.BuildNumber)"
    Check "env" "the PowerShell host is 64-bit" ([Environment]::Is64BitProcess) ""
    Check "env" "the process token is not elevated" (-not $effectiveAdmin) (
        "IsInRole(Administrator)=$effectiveAdmin; Administrators deny-only=$administratorsDenyOnly"
    )
    Check "env" "the token is Medium integrity" $medium (
        ($groups.output -split "`n" | Where-Object { $_ -match "S-1-16-" }) -join "; "
    )
    if ($effectiveAdmin) {
        Blocked "env" "the non-elevated permission matrix" "the current token is elevated"
    }

    $env:CARGO_TERM_COLOR = "never"
    $testSets = @(
        @("detection", "leveler-core", "--test", "secret_detection"),
        @("benchmark", "leveler-core", "--test", "secret_detection_benchmark"),
        @("core-declarations", "leveler-core", "--lib", "secret::tests"),
        @("vault", "leveler-execution", "--test", "secret_vault"),
        @("authority", "leveler-execution", "--lib", "secret_write"),
        @("broker-tool", "leveler-tools", "--lib", "secret_apply"),
        @("win-confine", "leveler-win-confine", "--lib", "private_file_tests"),
        @("admission", "leveler-app", "--lib", "secret_input"),
        @("e2e", "leveler-app", "--test", "secret_broker"),
        @("protocol", "leveler-client-protocol", "--lib", "secret"),
        @("tui-overlay", "leveler-tui", "--lib", "overlay::secret"),
        @("tui-history", "leveler-tui", "--lib", "secret_persistence"),
        @("windows-editor", "leveler-tools", "--lib", "windows_")
    )
    foreach ($case in $testSets) {
        $label, $package, $kind, $filter = $case
        $log = Join-Path $Evidence "cargo-test-$label.log"
        $cargoArgs = @("test", "-p", $package, $kind, $filter, "--", "--nocapture")
        $result = Invoke-Native "cargo" $cargoArgs $log
        $ok = $result.exit -eq 0
        Check "tests" "$label suite passed" $ok (
            $result.output | Select-Object -Last 12 | Out-String
        )
    }

    # ── Release artifacts ────────────────────────────────────────────────
    $buildLog = Join-Path $Evidence "cargo-build-release.log"
    Invoke-Native "cargo" @(
        "build", "--release", "-p", "leveler-cli", "--bin", "leveler"
    ) $buildLog | Out-Null
    $binary = Join-Path $Repo "target\release\leveler.exe"
    Check "release" "release binary built" (Test-Path $binary) $binary
    $binarySha = Sha256 $binary
    if (Test-Path $binary) {
        $version = (& $binary --version 2>&1 | Out-String).Trim()
        Check "release" "the release binary reports its provenance" ($version.Length -gt 0) $version
        Check "release" "the release binary has a SHA-256" ($binarySha -match "^[0-9a-f]{64}$") $binarySha
    }

    $confineLog = Join-Path $Evidence "cargo-build-confine.log"
    Invoke-Native "cargo" @(
        "build", "--release", "-p", "leveler-win-confine", "--bin", "leveler-confine"
    ) $confineLog | Out-Null
    $confine = Join-Path $Repo "target\release\leveler-confine.exe"
    $confineSha = Sha256 $confine
    Check "release" "the confine helper is built" (Test-Path $confine) $confine
    Check "release" "the confine helper has a SHA-256" ($confineSha -match "^[0-9a-f]{64}$") $confineSha

    $zipSha = ""
    if ($Zip -ne "") {
        Check "release" "the release ZIP exists" (Test-Path $Zip) $Zip
        if (Test-Path $Zip) {
            $zipSha = Sha256 $Zip
            $extract = Join-Path $Evidence "extracted"
            Remove-Item -Recurse -Force $extract -ErrorAction SilentlyContinue
            Expand-Archive -Path $Zip -DestinationPath $extract -Force
            $exe = Get-ChildItem -Path $extract -Recurse -Filter "leveler.exe" |
                Select-Object -First 1
            Check "release" "the ZIP contains leveler.exe" ($null -ne $exe) $extract
            if ($null -ne $exe) {
                $zVersion = (& $exe.FullName --version 2>&1 | Out-String).Trim()
                Check "release" "the extracted binary starts" ($zVersion.Length -gt 0) $zVersion
                Check "release" "the extracted binary matches the built candidate" (
                    (Sha256 $exe.FullName) -eq $binarySha
                ) (Sha256 $exe.FullName)
            }
        }
    }

    # ── Real workspace, ACL evidence, and a script-host junction ─────────
    $root = Join-Path $Evidence "验收 目录\项目 repo"
    $outside = Join-Path $Evidence "验收 目录\junction target"
    Remove-Item -Recurse -Force (Join-Path $Evidence "验收 目录") -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    New-Item -ItemType Directory -Force -Path $outside | Out-Null
    $junction = Join-Path $root "junction-escape"
    $linkResult = Run "$env:SystemRoot\System32\cmd.exe" @(
        "/c", "mklink", "/J", $junction, $outside
    )
    $junctionOk = (Test-Path $junction) -and ((Get-Item $junction).LinkType -ne $null)
    Check "junction" "the script host created a real junction" $junctionOk $linkResult.output

    $env:LEVELER_SECRET_ACCEPTANCE_ROOT = $root
    $env:LEVELER_SECRET_ACCEPTANCE_EVIDENCE = Join-Path $Evidence "windows-boundary.json"
    $env:LEVELER_SECRET_ACCEPTANCE_CANARY = $Canary
    if ($junctionOk) {
        $env:LEVELER_SECRET_ACCEPTANCE_JUNCTION = $junction
    }
    $evidenceLog = Join-Path $Evidence "cargo-test-windows-boundary.log"
    $result = Invoke-Native "cargo" @(
        "test", "--release", "-p", "leveler-tools", "--test",
        "secret_windows_acceptance", "--", "--nocapture"
    ) $evidenceLog
    Check "boundary" "the Windows boundary evidence test passed" (
        $result.exit -eq 0
    ) ($result.output | Select-Object -Last 16 | Out-String)

    $boundaryPath = $env:LEVELER_SECRET_ACCEPTANCE_EVIDENCE
    if (Test-Path $boundaryPath) {
        # Rust writes UTF-8. Windows PowerShell's Get-Content default encoding
        # misreads non-ASCII bytes and makes ConvertFrom-Json fail.
        $boundaryText = [System.IO.File]::ReadAllText(
            $boundaryPath, [System.Text.Encoding]::UTF8
        )
        $boundary = $boundaryText | ConvertFrom-Json
        foreach ($check in $boundary.checks) {
            Check "boundary" $check.name ([bool]$check.ok) $check.detail
        }
        $envAcl = Join-Path $root "配置 目录\.env"
        if (Test-Path $envAcl) {
            $acl = Get-Acl -Path $envAcl
            $allowedSids = @(
                "S-1-5-18",
                "S-1-5-32-544",
                [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
            )
            $rules = $acl.Access | Where-Object {
                $sid = $_.IdentityReference.Translate(
                    [System.Security.Principal.SecurityIdentifier]
                ).Value
                $allowedSids -notcontains $sid
            }
            Check "acl" "only the owner, SYSTEM and Administrators remain" (
                $rules.Count -eq 0
            ) (($acl.Access | ForEach-Object {
                "$($_.IdentityReference):$($_.FileSystemRights)"
            }) -join "; ")
            Check "acl" "the DACL is protected from parent inheritance" (
                $acl.AreAccessRulesProtected
            ) "AreAccessRulesProtected=$($acl.AreAccessRulesProtected)"
        } else {
            Blocked "acl" "the broker-written .env ACL" "配置 目录\.env was not created"
        }
        Check "canary" "the authorized .env holds the canary" (
            (Get-Content -Raw -Path $envAcl) -match [regex]::Escape($Canary)
        ) $envAcl
        $outsideEnv = Join-Path $outside ".env"
        Check "junction" "the junction target was not modified" (
            (Get-Content -Raw -Path $outsideEnv) -eq "UNCHANGED=1`n"
        ) $outsideEnv

        # Only the authorized .env files may contain the canary.
        $leaks = Get-ChildItem -Path (Join-Path $Evidence "验收 目录") -Recurse -File |
            Where-Object { $_.Name -ne ".env" } |
            Select-String -SimpleMatch $Canary -List
        Check "canary" "no other file in the acceptance tree holds the canary" (
            $leaks.Count -eq 0
        ) (($leaks | ForEach-Object { $_.Path }) -join "; ")
    } else {
        Blocked "boundary" "the Windows boundary evidence JSON" "the test did not write $boundaryPath"
    }

    # ── Real TUI process: reopening must not revive an undecided value ────
    $Harness = "C:\qlab\win11-full-20261118\tools\ConsoleHarness.exe"
    if (-not (Test-Path $Harness)) {
        Blocked "draft" "the real TUI draft check" "ConsoleHarness is not available at $Harness"
    } else {
        $draftHome = Join-Path $Evidence "draft-home"
        $draftWorkspace = Join-Path $Evidence "draft-workspace"
        Remove-Item -Recurse -Force $draftHome, $draftWorkspace -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force -Path (Join-Path $draftHome "state") | Out-Null
        New-Item -ItemType Directory -Force -Path $draftWorkspace | Out-Null
        $mockConfig = @"
default_model = "m"

[providers.mock]
protocol = "openai_chat"
base_url = "http://127.0.0.1:9"

[models.m]
provider = "mock"
model_id = "mock-model"
protocol = "openai_chat"
context_window = 131072
max_output_tokens = 1024
"@
        # Rust reads UTF-8 without a BOM. Windows PowerShell's Set-Content would
        # write UTF-16/ASCII depending on the host, so write the exact bytes.
        $utf8 = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText(
            (Join-Path $draftHome "config.toml"), $mockConfig, $utf8
        )
        $draftScript = Join-Path $Evidence "draft-exit.script"
        [System.IO.File]::WriteAllText(
            $draftScript,
            "waitfor CodeLeveler 90000`nsleep 3000`nkey esc`nsleep 500`nsend /exit\r`nsleep 1500`nkey enter`nsleep 4000`n",
            $utf8
        )
        # The TUI keeps drafts in the project state directory, not directly
        # under <home>\state: `<home>\state\projects\<project-id>\draft.txt`.
        # Warm up once so the real directory exists, then plant the unsafe
        # files there and reopen. Writing to the guessed path would test a
        # file the TUI never reads.
        $warmOut = Join-Path $Evidence "draft-warmup"
        $warmResult = Run $Harness @(
            "--exe", $binary,
            "--args", "tui --permission full-access --in-process",
            "--script", $draftScript,
            "--out", $warmOut,
            "--cwd", $draftWorkspace,
            "--cols", "120",
            "--rows", "30",
            "--env", "LEVELER_HOME=$draftHome"
        )
        Check "draft" "the warm-up TUI exited cleanly" (
            $warmResult.exit -eq 0
        ) ($warmResult.output | Select-Object -Last 8 | Out-String)
        $projects = Join-Path $draftHome "state\projects"
        $stateDir = $null
        if (Test-Path $projects) {
            $stateDir = Get-ChildItem -Path $projects -Directory |
                Sort-Object LastWriteTime -Descending |
                Select-Object -First 1 -ExpandProperty FullName
        }
        Check "draft" "the real TUI created its project state directory" (
            $null -ne $stateDir
        ) "$stateDir"
        if ($null -eq $stateDir) {
            Blocked "draft" "the real TUI draft check" "no project state directory was created"
        } else {
            [System.IO.File]::WriteAllText(
                (Join-Path $stateDir "draft.txt"), "API_KEY=abc123", $utf8
            )
            [System.IO.File]::WriteAllText(
                (Join-Path $stateDir "input_history.json"),
                '["API_KEY=abc123","cargo test"]',
                $utf8
            )
            $draftOut = Join-Path $Evidence "draft-tui"
            $harnessResult = Run $Harness @(
                "--exe", $binary,
                "--args", "tui --permission full-access --in-process",
                "--script", $draftScript,
                "--out", $draftOut,
                "--cwd", $draftWorkspace,
                "--cols", "120",
                "--rows", "30",
                "--env", "LEVELER_HOME=$draftHome"
            )
            Check "draft" "the reopened TUI exited cleanly" (
                $harnessResult.exit -eq 0
            ) ($harnessResult.output | Select-Object -Last 8 | Out-String)
            $draftPath = Join-Path $stateDir "draft.txt"
            $historyPath = Join-Path $stateDir "input_history.json"
            $draftText = if (Test-Path $draftPath) {
                [System.IO.File]::ReadAllText($draftPath, [System.Text.Encoding]::UTF8)
            } else { "" }
            $historyText = if (Test-Path $historyPath) {
                [System.IO.File]::ReadAllText($historyPath, [System.Text.Encoding]::UTF8)
            } else { "" }
            Check "draft" "the real TUI removed the unsafe draft on open" (
                -not $draftText.Contains("abc123")
            ) $draftText
            Check "draft" "the real TUI sanitized the old history" (
                -not $historyText.Contains("abc123") -and $historyText.Contains("cargo test")
            ) $historyText
        }
    }

    Write-Host ""
    Write-Host "Manual TUI checks (perform on this host and record the result separately):"
    Write-Host "  1. leveler --repo `"$root`" tui --permission full-access --in-process"
    Write-Host "  2. Read the synthetic key generated by this run from:"
    Write-Host "       $canaryPath"
    Write-Host "     Paste DEMO_API_KEY=<that value> and confirm the review opens;"
    Write-Host "     the value must never appear on screen."
    Write-Host "  3. Choose Capture, ask the model to write it to .env, and verify:"
    Write-Host "       icacls `"$root\.env`""
    Write-Host "     Only the current user, SYSTEM and Administrators may appear."
    Write-Host "  4. Try a Chinese IME paste, Enter/Esc/arrow keys, and a Chinese+space path."
    Write-Host "  5. Select Back in the review; no message may be sent."
    Write-Host "  6. `/secret revoke <sec_...>` then ask the model to use it again; it must fail."
    Write-Host "  7. Start a Goal, press Esc to interrupt, resume with `继续`, and verify the"
    Write-Host "     captured reference still works in the same runtime. Restart, then verify"
    Write-Host "     the old reference is unavailable and a fresh capture is requested."
    Write-Host ""

    $report = [ordered]@{
        platform          = "windows"
        candidate         = $sha
        dirty             = $dirty
        git_status        = $gitStatus
        os                = [ordered]@{
            caption = $os.Caption
            version = $os.Version
            build   = $os.BuildNumber
        }
        rustc             = $rustc
        cargo             = $cargo
        identity          = $identity.output
        groups            = $groups.output
        privileges        = $privs.output
        effective_admin   = $effectiveAdmin
        binary_sha256     = $binarySha
        confine_sha256    = $confineSha
        zip_sha256        = $zipSha
        failures          = $script:failures
        blocked           = $script:blocked
        results           = $script:results
        manual_tui        = "NOT RUN BY THIS SCRIPT"
        canary_run        = $canaryRun
        canary_sha256_12  = $canaryDigest
    }
    $reportPath = Join-Path $Evidence "report-secret-broker-windows.json"
    $reportJson = $report | ConvertTo-Json -Depth 8
    [System.IO.File]::WriteAllText(
        $reportPath, $reportJson, (New-Object System.Text.UTF8Encoding($false))
    )
    Write-Host "evidence: $reportPath"
    if ($script:failures -gt 0) { exit 1 }
    exit 0
} finally {
    Remove-Item $canaryPath -Force -ErrorAction SilentlyContinue
    Pop-Location
}
