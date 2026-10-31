#requires -Version 5.1
<#
.SYNOPSIS
    Full release acceptance for LFM3 Workspace.

.DESCRIPTION
    Runs the Rust and UI gates in order, builds the NSIS bundle, and then drives
    the installed-application probes from scripts/smoke.ps1:

      1. cargo fmt --check
      2. cargo clippy --workspace --all-targets -D warnings
      3. cargo test --workspace
      4. no Python project files anywhere in the source tree
      5. UI typecheck, unit tests, and Playwright end-to-end tests
      6. Tauri release build with the NSIS bundle
      7. installed smoke: real engine, process ownership, interruption

    Every step must exit 0. The installed smoke needs the local GGUF weights and
    the pinned llama.cpp b11960 / v0.8.0 runtime.

    Semantic qualification is explicitly NOT performed by this script. Every
    packaged semantic mission declares `human_review_only`, the engine refuses
    to start those missions, and the UI disables Start. Automatic semantic
    passing stays disabled until a frozen holdout is evaluated with at least
    three packaged runs and zero confident false completions.

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/accept.ps1

.EXAMPLE
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/accept.ps1 `
        -ModelsDir 'F:\lfm3\models' -ReuseInstall -SkipE2e
#>
[CmdletBinding()]
param(
    [switch]$SkipUi,
    [switch]$SkipE2e,
    [switch]$SkipBuild,
    [switch]$SkipSmoke,
    [switch]$ReuseInstall,
    [string]$ModelsDir,
    [string]$LlamaServer,
    [string]$InstallDir,
    [int]$PhaseTimeoutSeconds = 900
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$global:LASTEXITCODE = 0

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$root = Split-Path -Parent $scriptRoot
$ui = Join-Path $root 'apps\ui'
$desktop = Join-Path $root 'apps\desktop'
$rustup = Join-Path $env:USERPROFILE '.cargo\bin\rustup.exe'
if (-not (Test-Path $rustup)) {
    throw "rustup was not found at $rustup"
}
# npx tauri shells out to `cargo`, which is not always inherited on PATH even
# when rustup itself is installed. Keep every cargo invocation on the same
# rustup proxy.
$cargoBin = Split-Path -Parent $rustup
if (($env:PATH -split ';') -notcontains $cargoBin) {
    $env:PATH = "$cargoBin;$env:PATH"
}

$results = New-Object System.Collections.Generic.List[object]
$started = Get-Date

function Invoke-Step {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][scriptblock]$Body
    )
    Write-Host ''
    Write-Host "==> $Name" -ForegroundColor Cyan
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    & $Body
    if ($global:LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $global:LASTEXITCODE"
    }
    $watch.Stop()
    $results.Add([pscustomobject]@{
        Step    = $Name
        Seconds = [math]::Round($watch.Elapsed.TotalSeconds, 1)
    })
    Write-Host "    ok ($([math]::Round($watch.Elapsed.TotalSeconds, 1)) s)" -ForegroundColor Green
}

Invoke-Step 'cargo fmt --check' {
    & $rustup run stable cargo fmt --all -- --check
}

Invoke-Step 'cargo clippy' {
    & $rustup run stable cargo clippy --workspace --all-targets -- -D warnings
}

Invoke-Step 'cargo test --workspace' {
    & $rustup run stable cargo test --workspace
}

Invoke-Step 'no Python project files' {
    $patterns = @('*.py', 'pyproject.toml', 'requirements*.txt', 'Pipfile', 'setup.py', 'setup.cfg')
    $found = @(
        Get-ChildItem -Path $root -Recurse -File -Include $patterns -ErrorAction SilentlyContinue |
            Where-Object {
                $_.FullName -notmatch '\\target\\' -and
                $_.FullName -notmatch '\\node_modules\\'
            }
    )
    if ($found.Count -gt 0) {
        $found | ForEach-Object { Write-Host "    $($_.FullName)" }
        throw 'Python project files remain in the source tree'
    }
    $global:LASTEXITCODE = 0
}

if (-not $SkipUi) {
    Invoke-Step 'UI typecheck' {
        Push-Location $ui
        try { npm run typecheck } finally { Pop-Location }
    }
    Invoke-Step 'UI unit tests' {
        Push-Location $ui
        try { npm test } finally { Pop-Location }
    }
    if (-not $SkipE2e) {
        Invoke-Step 'Playwright end-to-end' {
            Push-Location $ui
            try { npx playwright test } finally { Pop-Location }
        }
    }
}

if (-not $SkipBuild) {
    Invoke-Step 'Tauri NSIS build' {
        Push-Location $desktop
        try { npx tauri build --bundles nsis } finally { Pop-Location }
    }
}

if (-not $SkipSmoke) {
    Invoke-Step 'installed smoke acceptance' {
        $smoke = Join-Path $scriptRoot 'smoke.ps1'
        $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $smoke)
        if ($ReuseInstall) { $arguments += '-ReuseInstall' }
        if ($ModelsDir) { $arguments += @('-ModelsDir', $ModelsDir) }
        if ($LlamaServer) { $arguments += @('-LlamaServer', $LlamaServer) }
        if ($InstallDir) { $arguments += @('-InstallDir', $InstallDir) }
        $arguments += @('-PhaseTimeoutSeconds', $PhaseTimeoutSeconds)
        & powershell @arguments
    }
}

$elapsed = (Get-Date) - $started
Write-Host ''
Write-Host '==> acceptance summary' -ForegroundColor Cyan
$results | Format-Table -AutoSize | Out-String | Write-Host

Write-Host ''
Write-Host 'Semantic qualification: NOT PERFORMED.' -ForegroundColor Yellow
Write-Host 'screen_qa, voice_intake, and doc_extract remain human_review_only.'
Write-Host 'The engine refuses to start them and the UI disables Start.'
Write-Host 'Automatic semantic passing stays disabled until a frozen holdout is'
Write-Host 'evaluated with at least three packaged runs and zero false completions.'
Write-Host ''
Write-Host ("Acceptance passed in {0:N1} s." -f $elapsed.TotalSeconds) -ForegroundColor Green
exit 0
