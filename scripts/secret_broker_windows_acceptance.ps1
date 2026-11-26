# Secret Broker 1.0 final closure — Windows 11 acceptance.
#
# Run from the repository root on a real Windows 11 host:
#
#   pwsh -File scripts\secret_broker_windows_acceptance.ps1 `
#        -Repo (Get-Location) [-Zip C:\path\leveler-windows-x86_64.zip]
#
# The script builds and runs the security-critical suites, then records the
# facts a macOS run cannot prove: the real file ACL on the written `.env`, a
# Chinese-and-space path, a junction outside the workspace, and the extracted
# release binary. The interactive TUI steps are printed and must be performed
# and reported separately; this script never marks them PASS on its own.
#
# Exit 0 = every automated check held. Exit 1 = a check failed. Exit 2 = the
# required toolchain/host is unavailable (BLOCKED, not PASS).

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
    $sha = (git rev-parse HEAD).Trim()
    $dirty = (git status --porcelain | Measure-Object -Line).Lines -gt 0
    Check "env" "candidate SHA recorded" ($sha -match "^[0-9a-f]{40}$") $sha
    Check "env" "working tree is clean" (-not $dirty) "uncommitted changes present"
    Check "env" "the Rust toolchain is installed" ([bool](Get-Command cargo -ErrorAction SilentlyContinue)) ""
    Check "env" "the .NET/PowerShell host is 64-bit" ([Environment]::Is64BitProcess) ""

    $env:CARGO_TERM_COLOR = "never"
    $tests = @(
        @("detection", "leveler-core", "--test", "secret_detection"),
        @("benchmark", "leveler-core", "--test", "secret_detection_benchmark"),
        @("vault", "leveler-execution", "--test", "secret_vault"),
        @("authority", "leveler-execution", "--lib", "secret_write"),
        @("broker-tool", "leveler-tools", "--lib", "secret_apply"),
        @("admission", "leveler-app", "--lib", "secret_input"),
        @("e2e", "leveler-app", "--test", "secret_broker"),
        @("protocol", "leveler-client-protocol", "--lib", "secret"),
        @("tui-overlay", "leveler-tui", "--lib", "overlay::secret"),
        @("windows-editor", "leveler-tools", "--lib", "windows_")
    )
    foreach ($case in $tests) {
        $label, $package, $kind, $filter = $case
        $log = Join-Path $Evidence "cargo-test-$label.log"
        $cargoArgs = @("test", "-p", $package, $kind, $filter, "--", "--nocapture")
        $output = & cargo @cargoArgs 2>&1 | Tee-Object -FilePath $log
        $ok = $LASTEXITCODE -eq 0
        Check "tests" "$label suite passed" $ok ($output | Select-Object -Last 12 | Out-String)
    }

    $buildLog = Join-Path $Evidence "cargo-build-release.log"
    & cargo build --release -p leveler-cli --bin leveler 2>&1 |
        Tee-Object -FilePath $buildLog | Out-Null
    $binary = Join-Path $Repo "target\release\leveler.exe"
    Check "release" "release binary built" (Test-Path $binary) $binary
    if (Test-Path $binary) {
        $version = (& $binary --version 2>&1 | Out-String).Trim()
        Check "release" "the release binary reports its provenance" ($version.Length -gt 0) $version
    }

    if ($Zip -ne "") {
        Check "release" "the release ZIP exists" (Test-Path $Zip) $Zip
        if (Test-Path $Zip) {
            $extract = Join-Path $Evidence "extracted"
            Remove-Item -Recurse -Force $extract -ErrorAction SilentlyContinue
            Expand-Archive -Path $Zip -DestinationPath $extract -Force
            $exe = Get-ChildItem -Path $extract -Recurse -Filter "leveler.exe" |
                Select-Object -First 1
            Check "release" "the ZIP contains leveler.exe" ($null -ne $exe) $extract
            if ($null -ne $exe) {
                $zVersion = (& $exe.FullName --version 2>&1 | Out-String).Trim()
                Check "release" "the extracted binary starts" ($zVersion.Length -gt 0) $zVersion
            }
        }
    }

    # A real workspace path with Chinese characters and spaces. The tool test
    # exercises this on every platform; the ACL check below applies only after
    # the TUI manual step has written this workspace's .env through the broker.
    $root = Join-Path $Evidence "验收 目录"
    $ws = Join-Path $root "项目 repo"
    New-Item -ItemType Directory -Force -Path $ws | Out-Null
    $envPath = Join-Path $ws ".env"
    if (Test-Path $envPath) {
        $acl = Get-Acl -Path $envPath
        $allowedSids = @(
            "S-1-5-18",      # NT AUTHORITY\SYSTEM
            "S-1-5-32-544",  # BUILTIN\Administrators
            [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        )
        $rules = $acl.Access | Where-Object {
            $sid = $_.IdentityReference.Translate(
                [System.Security.Principal.SecurityIdentifier]
            ).Value
            $allowedSids -notcontains $sid
        }
        Check "acl" "only the owner, SYSTEM and Administrators remain" ($rules.Count -eq 0) (
            ($acl.Access | ForEach-Object {
                "$($_.IdentityReference):$($_.FileSystemRights)"
            }) -join "; "
        )
        Check "acl" "the DACL is protected from parent inheritance" $acl.AreAccessRulesProtected (
            "AreAccessRulesProtected=$($acl.AreAccessRulesProtected)"
        )
    } else {
        Blocked "acl" "the broker-written .env ACL" (
            "run the TUI manual step for `"$ws`" first; the Rust Windows test " +
            "secret_apply::tests::the_dotenv_gets_an_owner_only_acl already " +
            "asserts the DACL in an isolated workspace"
        )
    }

    # A junction that leaves the workspace must not redirect the broker. The
    # Rust Windows editor tests cover this in-process; this is the same fact
    # at the file-system level for the report.
    $outside = Join-Path $Evidence "outside"
    New-Item -ItemType Directory -Force -Path $outside | Out-Null
    $junction = Join-Path $ws "escape"
    cmd /c mklink /J "$junction" "$outside" | Out-Null
    Check "junction" "a junction was created for the escape probe" (Test-Path $junction) $junction
    if (Test-Path $junction) {
        $target = Join-Path $junction ".env"
        Set-Content -Path $target -Value "UNCHANGED=1" -NoNewline
        $outsideEnv = Join-Path $outside ".env"
        Check "junction" "the junction resolves outside the workspace" (
            (Resolve-Path $junction).Path -ne (Resolve-Path $ws).Path
        ) $junction
        Check "junction" "the outside file is not the workspace .env" (
            (Test-Path $outsideEnv) -and ((Get-Content $outsideEnv -Raw) -eq "UNCHANGED=1")
        ) $outsideEnv
        Remove-Item $junction -Force
    }

    Write-Host ""
    Write-Host "Manual TUI checks (perform on this host and record the result separately):"
    Write-Host "  1. leveler --repo `"$ws`" tui --permission full-access --in-process"
    Write-Host "  2. Read the synthetic key generated by this run from:"
    Write-Host "       $canaryPath"
    Write-Host "     Paste DEMO_API_KEY=<that value> and confirm the review opens;"
    Write-Host "     the value must never appear on screen."
    Write-Host "  3. Choose Capture, ask the model to write it to .env, and verify:"
    Write-Host "       icacls `"$envPath`""
    Write-Host "     Only the current user, SYSTEM and Administrators may appear."
    Write-Host "  4. Try a Chinese IME paste, Enter/Esc/arrow keys, and a Chinese+space path."
    Write-Host "  5. Select Back in the review; no message may be sent."
    Write-Host "  6. `/secret revoke <sec_...>` then ask the model to use it again; it must fail."
    Write-Host ""

    $report = [ordered]@{
        platform   = "windows"
        candidate  = $sha
        dirty      = $dirty
        failures   = $script:failures
        blocked    = $script:blocked
        results    = $script:results
        manual_tui = "NOT RUN BY THIS SCRIPT"
    }
    $reportPath = Join-Path $Evidence "report-secret-broker-windows.json"
    $report | ConvertTo-Json -Depth 6 | Set-Content -Path $reportPath
    Write-Host "evidence: $reportPath"
    if ($script:failures -gt 0) { exit 1 }
    exit 0
} finally {
    Remove-Item $canaryPath -Force -ErrorAction SilentlyContinue
    Pop-Location
}
