#requires -Version 5.1
<#
.SYNOPSIS
    Installed-application acceptance for LFM3 Workspace.

.DESCRIPTION
    Installs the NSIS build silently into a path containing spaces, then runs
    the packaged executable's embedded-engine probes:

      --smoke        starts the real desk_task, proves an overlapping start is
                     refused, cancels it, starts and cancels a second run,
                     releases the model
      --smoke-close  starts a run, then closes the engine; the next boot must
                     report the run as interrupted with no active run
      --smoke-hold   holds a run open until the script force-terminates the
                     process; the owned llama-server must die with the job

    An externally started llama-server is kept running throughout and must
    survive every phase, proving job ownership only covers harness children.
    The installed missions resource is used (LFM3_MISSIONS_DIR is removed), so
    nothing resolves back to the source tree.

    Exit code 0 means every phase passed and no owned process survived.
#>
[CmdletBinding()]
param(
    [string]$Installer,
    [string]$InstallDir,
    [string]$ModelsDir,
    [string]$LlamaServer,
    [string]$DataDir,
    [string]$ReportDir,
    [switch]$ReuseInstall,
    [int]$PhaseTimeoutSeconds = 900
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# $PSScriptRoot is not reliably populated while parameter defaults bind under
# Windows PowerShell, so resolve the script root and defaults here instead.
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Installer) {
    $Installer = Join-Path $scriptRoot '..\target\release\bundle\nsis\LFM3 Workspace_0.1.0_x64-setup.exe'
}
if (-not $InstallDir) {
    $InstallDir = Join-Path $env:LOCALAPPDATA 'LFM3 Workspace'
}
if (-not $ModelsDir) {
    $ModelsDir = Join-Path $scriptRoot '..\models'
}
if (-not $LlamaServer) {
    $LlamaServer = Join-Path $scriptRoot '..\tools\llama\llama-server.exe'
}
if (-not $DataDir) {
    $DataDir = Join-Path $env:TEMP 'LFM3 Smoke Data'
}
if (-not $ReportDir) {
    $ReportDir = Join-Path $env:TEMP 'LFM3 Smoke Reports'
}

$script:failures = New-Object System.Collections.Generic.List[string]

function Write-Phase([string]$Message) {
    Write-Host ""
    Write-Host "== $Message" -ForegroundColor Cyan
}

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) {
        $script:failures.Add($Message)
        Write-Host "FAIL $Message" -ForegroundColor Red
    } else {
        Write-Host "ok   $Message" -ForegroundColor Green
    }
}

function Get-LlamaPids {
    @(Get-Process -Name 'llama-server' -ErrorAction SilentlyContinue |
        Select-Object -ExpandProperty Id)
}

function Get-NewPids([int[]]$Baseline) {
    @(Get-LlamaPids | Where-Object { $Baseline -notcontains $_ })
}

function Wait-NewPidsGone([int[]]$Baseline, [int]$Seconds) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $remaining = @(Get-NewPids $Baseline)
        if ($remaining.Count -eq 0) { return $true }
        Start-Sleep -Milliseconds 500
    }
    $remaining = @(Get-NewPids $Baseline)
    return ($remaining.Count -eq 0)
}

function Remove-IfExists([string]$Path) {
    if (Test-Path -LiteralPath $Path) {
        Remove-Item -LiteralPath $Path -Recurse -Force
    }
}

function Read-JsonReport([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    try {
        return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    } catch {
        return $null
    }
}

function Wait-ForReport([string]$Path, [System.Diagnostics.Process]$Process, [int]$Seconds) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        $report = Read-JsonReport $Path
        if ($null -ne $report) { return $report }
        if ($Process.HasExited) {
            Start-Sleep -Milliseconds 500
            return (Read-JsonReport $Path)
        }
        Start-Sleep -Milliseconds 500
    }
    return $null
}

function Invoke-Probe([string]$Mode, [string]$ReportPath) {
    Remove-IfExists $ReportPath
    $env:LFM3_SMOKE_REPORT = $ReportPath
    $process = Start-Process -FilePath $script:exePath -ArgumentList $Mode -PassThru
    if (-not $process.WaitForExit($PhaseTimeoutSeconds * 1000)) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        throw "$Mode did not finish within $PhaseTimeoutSeconds seconds"
    }
    $report = Wait-ForReport $ReportPath $process 5
    return [pscustomobject]@{
        Process = $process
        Report  = $report
    }
}

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

if (-not $ReuseInstall) {
    if (-not (Test-Path -LiteralPath $Installer)) {
        throw "installer not found: $Installer (build with: npx tauri build --bundles nsis)"
    }
}
if (-not (Test-Path -LiteralPath $LlamaServer)) {
    throw "pinned llama-server not found: $LlamaServer"
}
if (-not (Test-Path -LiteralPath $ModelsDir)) {
    throw "models directory not found: $ModelsDir"
}

# ---------------------------------------------------------------------------
# Install into a path containing spaces
# ---------------------------------------------------------------------------

Write-Phase "Install"

$script:exePath = Join-Path $InstallDir 'LFM3 Workspace.exe'
if (-not (Test-Path -LiteralPath $script:exePath)) {
    # Tauri installs the binary under its Cargo target name; productName only
    # affects the shortcut and Add/Remove entry.
    $script:exePath = Join-Path $InstallDir 'lfm3-desktop.exe'
}
if (-not $ReuseInstall) {
    Remove-IfExists $InstallDir
    $install = Start-Process -FilePath $Installer -ArgumentList '/S' -Wait -PassThru
    Assert-True ($install.ExitCode -eq 0) "silent installer exited 0 (actual $($install.ExitCode))"
}

# Re-resolve after install in case the bundle renamed the binary.
if (-not (Test-Path -LiteralPath $script:exePath)) {
    $script:exePath = Join-Path $InstallDir 'lfm3-desktop.exe'
}

Assert-True ($InstallDir -match ' ') "install path contains a space: $InstallDir"
Assert-True (Test-Path -LiteralPath $script:exePath) "installed executable exists: $script:exePath"

$missionsPath = Join-Path $InstallDir 'missions'
if (-not (Test-Path -LiteralPath (Join-Path $missionsPath 'desk_task.json'))) {
    $resourcesPath = Join-Path $InstallDir 'resources\missions'
    if (Test-Path -LiteralPath (Join-Path $resourcesPath 'desk_task.json')) {
        $missionsPath = $resourcesPath
    }
}
Assert-True (Test-Path -LiteralPath (Join-Path $missionsPath 'desk_task.json')) `
    "installed missions resource is present: $missionsPath"

$pythonFiles = @(Get-ChildItem -LiteralPath $InstallDir -Recurse -File -Filter '*.py' -ErrorAction SilentlyContinue)
Assert-True ($pythonFiles.Count -eq 0) "installed tree contains no Python source ($($pythonFiles.Count) files)"

# ---------------------------------------------------------------------------
# Environment: local paths only, packaged missions, isolated trusted state
# ---------------------------------------------------------------------------

Write-Phase "Prepare isolated state"

Remove-IfExists $DataDir
New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $DataDir 'workspace') -Force | Out-Null
Remove-IfExists $ReportDir
New-Item -ItemType Directory -Path $ReportDir -Force | Out-Null

$env:LFM3_DATA_DIR = $DataDir
$env:LFM3_WORKSPACE = Join-Path $DataDir 'workspace'
$env:LFM3_MODELS_DIR = (Resolve-Path -LiteralPath $ModelsDir).Path
$env:LFM3_LLAMA_SERVER = (Resolve-Path -LiteralPath $LlamaServer).Path
Remove-Item Env:LFM3_MISSIONS_DIR -ErrorAction SilentlyContinue
Remove-Item Env:LFM3_SMOKE_REPORT -ErrorAction SilentlyContinue

Write-Host "data:     $env:LFM3_DATA_DIR"
Write-Host "models:   $env:LFM3_MODELS_DIR"
Write-Host "runtime:  $env:LFM3_LLAMA_SERVER"
Write-Host "missions: packaged ($missionsPath)"

# ---------------------------------------------------------------------------
# External ownership probe: a llama-server this script owns, never the app's
# ---------------------------------------------------------------------------

Write-Phase "External ownership probe"

$baseline = @(Get-LlamaPids)
$portProbe = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$portProbe.Start()
$externalPort = $portProbe.LocalEndpoint.Port
$portProbe.Stop()
$external = Start-Process -FilePath $env:LFM3_LLAMA_SERVER `
    -ArgumentList @('--host', '127.0.0.1', '--port', "$externalPort", '--no-webui') `
    -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 3
Assert-True (-not $external.HasExited) "external llama-server is running (PID $($external.Id))"
if (-not $external.HasExited) {
    $baseline = @($baseline + $external.Id | Sort-Object -Unique)
}

try {
    # -----------------------------------------------------------------------
    # Phase 1: real run, overlapping start refused, cancel, second cancel
    # -----------------------------------------------------------------------

    Write-Phase "Phase 1: real run and cancellation"
    $checkReport = Join-Path $ReportDir 'check.json'
    $check = Invoke-Probe '--smoke' $checkReport
    Assert-True ($check.Process.ExitCode -eq 0) "--smoke exited 0 (actual $($check.Process.ExitCode))"
    Assert-True ($null -ne $check.Report -and $check.Report.ok -eq $true) `
        "--smoke report ok: $(if ($null -ne $check.Report) { $check.Report | ConvertTo-Json -Compress } else { 'no report' })"
    if ($null -ne $check.Report) {
        Assert-True ($check.Report.runtime_pinned -eq $true) "runtime identity is the pinned build"
        Assert-True ($check.Report.model_ready -eq $true) "the harness-owned text model became ready"
        Assert-True ($check.Report.overlapping_start_refused -eq $true) "an overlapping start was refused"
        Assert-True ($check.Report.first_cancelled -eq $true) "the first run reached cancelled"
        Assert-True ($check.Report.second_cancelled -eq $true) "the second run reached cancelled"
        Assert-True ($check.Report.model_released -eq $true) "the text model was released"
        Assert-True ($check.Report.text_model_absent_after_release -eq $true) "no model server remained after release"
    }
    Assert-True (Wait-NewPidsGone $baseline 30) "no owned llama-server survived the check phase"
    Assert-True (-not $external.HasExited) "external llama-server survived the check phase"

    # -----------------------------------------------------------------------
    # Phase 2: close during a run
    # -----------------------------------------------------------------------

    Write-Phase "Phase 2: close during another run"
    $closeReport = Join-Path $ReportDir 'close.json'
    $close = Invoke-Probe '--smoke-close' $closeReport
    Assert-True ($close.Process.ExitCode -eq 0) "--smoke-close exited 0 (actual $($close.Process.ExitCode))"
    Assert-True ($null -ne $close.Report -and $close.Report.ok -eq $true) `
        "--smoke-close report ok: $(if ($null -ne $close.Report) { $close.Report | ConvertTo-Json -Compress } else { 'no report' })"
    if ($null -ne $close.Report) {
        Assert-True ($close.Report.interrupted -eq $true) "the closed run is reported interrupted"
        Assert-True ($null -eq $close.Report.active_run_after_reopen) "no run is left active after reopen"
    }
    Assert-True (Wait-NewPidsGone $baseline 30) "no owned llama-server survived the close phase"
    Assert-True (-not $external.HasExited) "external llama-server survived the close phase"

    # -----------------------------------------------------------------------
    # Phase 3: force-terminate the application during a run
    # -----------------------------------------------------------------------

    Write-Phase "Phase 3: force termination"
    $holdReport = Join-Path $ReportDir 'hold.json'
    Remove-IfExists $holdReport
    $env:LFM3_SMOKE_REPORT = $holdReport
    $hold = Start-Process -FilePath $script:exePath -ArgumentList '--smoke-hold' -PassThru
    $holdState = Wait-ForReport $holdReport $hold 600
    Assert-True ($null -ne $holdState -and $holdState.ok -eq $true) `
        "--smoke-hold reported a live run: $(if ($null -ne $holdState) { $holdState | ConvertTo-Json -Compress } else { 'no report' })"

    $owned = @()
    $deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $deadline) {
        $owned = @(Get-NewPids $baseline)
        if ($owned.Count -gt 0) { break }
        Start-Sleep -Milliseconds 500
    }
    Assert-True ($owned.Count -gt 0) "the held run owned a llama-server process (PIDs $($owned -join ','))"
    Assert-True (-not $hold.HasExited) "the application was still running before force termination"

    # Force termination: no graceful close, no unwinding.
    Stop-Process -Id $hold.Id -Force
    $hold.WaitForExit(30000) | Out-Null
    Assert-True ($hold.HasExited) "the application process was terminated"
    Assert-True (Wait-NewPidsGone $baseline 30) `
        "force termination killed every owned llama-server (survivors: $((Get-NewPids $baseline) -join ','))"
    Assert-True (-not $external.HasExited) "external llama-server survived force termination"

    # -----------------------------------------------------------------------
    # Truthful state after forced termination
    # -----------------------------------------------------------------------

    Write-Phase "Phase 4: state after forced termination"
    $recoverReport = Join-Path $ReportDir 'recover.json'
    $recover = Invoke-Probe '--smoke' $recoverReport
    Assert-True ($recover.Process.ExitCode -eq 0) "reopen exited 0 (actual $($recover.Process.ExitCode))"
    Assert-True ($null -ne $recover.Report -and $recover.Report.ok -eq $true) `
        "reopen report ok: $(if ($null -ne $recover.Report) { $recover.Report | ConvertTo-Json -Compress } else { 'no report' })"
    Assert-True (Wait-NewPidsGone $baseline 30) "no owned llama-server survived the reopen phase"
} finally {
    if ($null -ne $external -and -not $external.HasExited) {
        Stop-Process -Id $external.Id -Force -ErrorAction SilentlyContinue
    }
}

# ---------------------------------------------------------------------------
# Result
# ---------------------------------------------------------------------------

Write-Phase "Result"
if ($script:failures.Count -gt 0) {
    Write-Host ""
    Write-Host "$($script:failures.Count) acceptance check(s) failed:" -ForegroundColor Red
    foreach ($failure in $script:failures) {
        Write-Host " - $failure" -ForegroundColor Red
    }
    exit 1
}

Write-Host "Installed smoke acceptance passed: $InstallDir" -ForegroundColor Green
exit 0
