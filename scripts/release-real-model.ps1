#requires -Version 5.1
<#
.SYNOPSIS
    Packaged real-model adversarial acceptance for the release build.

.DESCRIPTION
    Drives the optimized `lfm3-cli` against the real pinned models and writes
    one JSON result per case into the release evidence directory:

      success      real desk_task pass with a measured peak model memory
      failure      unsatisfiable required term ends incomplete (no_progress)
      escape       output path outside the workspace is rejected before planning
      retry        a transient write failure recovers on a later attempt
      crash        the owned llama-server is killed mid-run; no false pass
      unavailable  an empty loopback endpoint fails without spawning a server
      timeout      a one-second whole-run deadline cancels an owned server
      missing      an empty models directory fails without downloading anything

    Every case starts from a fresh workspace, database, and data directory.
    Event journals, reports, stderr, and result JSON land in tests/golden/release.
#>
[CmdletBinding()]
param(
    [string]$Root,
    [string]$EvidenceDir,
    [string]$Cli,
    [string]$ModelsDir,
    [string]$LlamaServer,
    [string]$MissionInputs,
    [int]$CaseTimeoutSeconds = 900
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Split-Path -Parent $scriptRoot
if (-not $Root) { $Root = Join-Path $env:TEMP 'LFM3 Release Acceptance' }
if (-not $EvidenceDir) { $EvidenceDir = Join-Path $repo 'tests\golden\release' }
if (-not $Cli) { $Cli = Join-Path $repo 'target\release\lfm3-cli.exe' }
if (-not $ModelsDir) { $ModelsDir = Join-Path $repo 'models' }
if (-not $LlamaServer) { $LlamaServer = Join-Path $repo 'tools\llama\llama-server.exe' }
if (-not $MissionInputs) { $MissionInputs = Join-Path $repo 'tests\fixtures\desk_task' }
$fixtureWorkspace = Join-Path $MissionInputs 'workspace'

foreach ($path in @($Cli, $LlamaServer, $fixtureWorkspace)) {
    if (-not (Test-Path -LiteralPath $path)) { throw "missing prerequisite: $path" }
}
New-Item -ItemType Directory -Path $Root -Force | Out-Null
New-Item -ItemType Directory -Path $EvidenceDir -Force | Out-Null

$script:baseline = @(Get-Process -Name 'llama-server' -ErrorAction SilentlyContinue |
    Select-Object -ExpandProperty Id)

function Get-Sha256([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function New-Workspace([string]$Name) {
    $workspace = Join-Path $Root $Name
    if (Test-Path -LiteralPath $workspace) { Remove-Item -LiteralPath $workspace -Recurse -Force }
    New-Item -ItemType Directory -Path $workspace -Force | Out-Null
    Copy-Item -Path (Join-Path $fixtureWorkspace '*') -Destination $workspace -Recurse -Force
    return $workspace
}

function Set-BaseEnvironment([string]$Name) {
    $env:LFM3_DATA_DIR = Join-Path $Root "state-$Name"
    $env:LFM3_MODELS_DIR = (Resolve-Path -LiteralPath $ModelsDir).Path
    $env:LFM3_LLAMA_SERVER = (Resolve-Path -LiteralPath $LlamaServer).Path
    Remove-Item Env:LFM3_PROFILE -ErrorAction SilentlyContinue
    Remove-Item Env:LFM3_TEXT_URL -ErrorAction SilentlyContinue
    Remove-Item Env:LFM3_MISSIONS_DIR -ErrorAction SilentlyContinue
    Remove-Item Env:LFM3_ALLOW_DOWNLOAD -ErrorAction SilentlyContinue
}

function Get-NewPids {
    @(Get-Process -Name 'llama-server' -ErrorAction SilentlyContinue |
        Where-Object { $script:baseline -notcontains $_.Id } |
        Select-Object -ExpandProperty Id)
}

function Wait-NewPidsGone([int]$Seconds) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        if (@(Get-NewPids).Count -eq 0) { return $true }
        Start-Sleep -Milliseconds 500
    }
    return (@(Get-NewPids).Count -eq 0)
}

function Read-JsonOrNull([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    try { return (Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json) } catch { return $null }
}

function Prop($Object, [string]$Name) {
    if ($null -eq $Object) { return $null }
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

function Get-EventMarks([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $text = Get-Content -LiteralPath $Path -Raw -ErrorAction SilentlyContinue
    if (-not $text) { return $null }
    return [pscustomobject]@{
        attempt_failed = ($text -match '"kind":"step\.failed"')
        step_replanned = ($text -match '"kind":"step\.replanned"')
        run_finished = ($text -match '"kind":"run\.finished"')
    }
}

function Invoke-Case {
    param(
        [Parameter(Mandatory)][string]$Name,
        [Parameter(Mandatory)][string[]]$Arguments,
        [scriptblock]$Watch,
        [string]$Notes = ''
    )
    Write-Host ''
    Write-Host "== $Name" -ForegroundColor Cyan
    $stdout = Join-Path $EvidenceDir "$Name.report.json"
    $stderr = Join-Path $EvidenceDir "$Name.stderr.txt"
    $events = Join-Path $EvidenceDir "$Name.events.jsonl"
    foreach ($path in @($stdout, $stderr, $events)) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Force }
    }

    $peak = 0
    $watchDone = $false
    $started = Get-Date
    # Start-Process joins ArgumentList with spaces and does not quote entries,
    # so paths under a directory with a space must be quoted here.
    $quoted = @($Arguments | ForEach-Object {
        if ($_ -match '\s') { '"' + $_ + '"' } else { $_ }
    }) -join ' '
    # PowerShell 5.1 loses the exit code of a redirected Start-Process child in
    # this environment, so a tiny batch wrapper records ERRORLEVEL itself.
    $exitFile = Join-Path $Root "$Name.exit.txt"
    if (Test-Path -LiteralPath $exitFile) { Remove-Item -LiteralPath $exitFile -Force }
    $wrapper = Join-Path $Root "$Name-run.cmd"
    Set-Content -LiteralPath $wrapper -Encoding ASCII -Value @(
        '@echo off',
        ('"' + $Cli + '" ' + $quoted),
        ('echo %ERRORLEVEL% > "' + $exitFile + '"')
    )
    $process = Start-Process -FilePath $wrapper -WindowStyle Hidden `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
    $deadline = $started.AddSeconds($CaseTimeoutSeconds)
    while (-not $process.HasExited) {
        foreach ($server in @(Get-Process -Name 'llama-server' -ErrorAction SilentlyContinue)) {
            if ($script:baseline -notcontains $server.Id -and $server.WorkingSet64 -gt $peak) {
                $peak = $server.WorkingSet64
            }
        }
        if ($null -ne $Watch -and -not $watchDone) {
            $watchDone = [bool](& $Watch)
        }
        if ((Get-Date) -gt $deadline) {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            throw "$Name exceeded $CaseTimeoutSeconds seconds"
        }
        Start-Sleep -Milliseconds 200
    }
    $process.WaitForExit()
    $seconds = ((Get-Date) - $started).TotalSeconds

    $recovered = Wait-NewPidsGone 30
    $exitCode = $null
    if (Test-Path -LiteralPath $exitFile) {
        $exitCode = [int]((Get-Content -LiteralPath $exitFile -Raw).Trim())
    }
    $report = Read-JsonOrNull $stdout
    $marks = Get-EventMarks $events
    $outcome = Prop $report 'outcome'
    $run = Prop $report 'run'
    $output = Prop $report 'output'
    $observed = Prop $output 'observed'
    $steps = @()
    $rawSteps = Prop $report 'steps'
    if ($null -ne $rawSteps) { $steps = @($rawSteps) }
    $attempts = $null
    if ($steps.Count -gt 0) {
        $attempts = (@($steps | ForEach-Object { @(Prop $_ 'attempts').Count }) | Measure-Object -Sum).Sum
    }
    $firstFailed = $null
    if ($steps.Count -gt 0) {
        $firstAttempts = @(Prop $steps[0] 'attempts')
        if ($firstAttempts.Count -gt 0) {
            $firstFailed = (Prop $firstAttempts[0] 'status') -ne 'passed'
        }
    }
    $result = [ordered]@{
        case               = $Name
        exit_code          = $exitCode
        wall_seconds       = [math]::Round($seconds, 2)
        peak_memory_kb     = [math]::Round($peak / 1KB)
        status             = Prop $outcome 'status'
        reason             = Prop $outcome 'reason'
        attempts           = $attempts
        acceptance_hash    = Prop $run 'acceptance_hash'
        output_sha256      = Prop $observed 'sha256'
        first_attempt_failed = $firstFailed
        step_replanned     = Prop $marks 'step_replanned'
        report             = "tests/golden/release/$Name.report.json"
        report_sha256      = Get-Sha256 $stdout
        events             = "tests/golden/release/$Name.events.jsonl"
        events_sha256      = Get-Sha256 $events
        stderr             = "tests/golden/release/$Name.stderr.txt"
        stderr_sha256      = Get-Sha256 $stderr
        leftover_pids      = @(Get-NewPids)
        owned_servers_gone = $recovered
        notes              = $Notes
    }
    $json = $result | ConvertTo-Json -Depth 6
    $json | Set-Content -LiteralPath (Join-Path $EvidenceDir "$Name.result.json") -Encoding UTF8
    Write-Host $json
    return [pscustomobject]$result
}

$summary = New-Object System.Collections.Generic.List[object]

# ---------------------------------------------------------------- success
$name = 'success'
Set-BaseEnvironment $name
$workspace = New-Workspace $name
$success = Invoke-Case -Name $name -Watch $null -Notes 'Real pinned text model completes the frozen desk_task contract.' -Arguments @(
    'run', '--mission', 'desk_task',
    '--input', (Join-Path $MissionInputs 'basic.json'),
    '--workspace', $workspace,
    '--db', (Join-Path $Root 'success.db'),
    '--events', (Join-Path $EvidenceDir 'success.events.jsonl')
)
$summary.Add($success)
if ($success.exit_code -ne 0 -or $success.status -ne 'passed') {
    throw 'success case did not pass truthfully'
}

# ---------------------------------------------------------------- failure
$name = 'failure'
Set-BaseEnvironment $name
$workspace = New-Workspace $name
$failure = Invoke-Case -Name $name -Watch $null -Notes 'A required term the source cannot support must exhaust attempts without passing.' -Arguments @(
    'run', '--mission', 'desk_task',
    '--input', (Join-Path $MissionInputs 'impossible_terms.json'),
    '--workspace', $workspace,
    '--db', (Join-Path $Root 'failure.db'),
    '--events', (Join-Path $EvidenceDir 'failure.events.jsonl')
)
$summary.Add($failure)
if ($failure.exit_code -eq 0 -or $failure.status -eq 'passed') {
    throw 'failure case passed; a deterministic acceptance check was weakened'
}
if ($failure.reason -ne 'no_progress') {
    throw "failure case ended with $($failure.reason); expected no_progress"
}

# ---------------------------------------------------------------- escape
$name = 'escape'
Set-BaseEnvironment $name
$workspace = New-Workspace $name
$escape = Invoke-Case -Name $name -Watch $null -Notes 'An output path outside the workspace is rejected before planning.' -Arguments @(
    'run', '--mission', 'desk_task',
    '--input', (Join-Path $MissionInputs 'escape_output.json'),
    '--workspace', $workspace,
    '--db', (Join-Path $Root 'escape.db'),
    '--events', (Join-Path $EvidenceDir 'escape.events.jsonl')
)
$summary.Add($escape)
if ($escape.exit_code -eq 0 -or $escape.status -eq 'passed') { throw 'escape case passed' }
if (Test-Path -LiteralPath (Join-Path $Root 'escaped.md')) { throw 'escape case wrote outside the workspace' }

# ---------------------------------------------------------------- retry
$name = 'retry'
Set-BaseEnvironment $name
$workspace = New-Workspace $name
$outDir = Join-Path $workspace 'out'
$target = Join-Path $outDir 'summary.md'
# The CLI removes stale output before planning, so remove it first and then
# deny CreateFiles on the output directory. The atomic temp-file write fails
# until the harness journals attempt.failed, at which point the deny rule is
# removed and the retried write succeeds.
if (Test-Path -LiteralPath $target) { Remove-Item -LiteralPath $target -Force }
$identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$denyRule = New-Object System.Security.AccessControl.FileSystemAccessRule(
    $identity,
    [System.Security.AccessControl.FileSystemRights]::CreateFiles,
    [System.Security.AccessControl.AccessControlType]::Deny
)
$acl = Get-Acl -LiteralPath $outDir
$acl.AddAccessRule($denyRule)
Set-Acl -LiteralPath $outDir -AclObject $acl
$retryEvents = Join-Path $EvidenceDir 'retry.events.jsonl'
$lockDone = Join-Path $Root 'retry-lock.done'
if (Test-Path -LiteralPath $lockDone) { Remove-Item -LiteralPath $lockDone -Force }
$unlockJob = Start-Job -ScriptBlock {
    param([string]$Dir, [string]$Events, [string]$Done, [string]$Identity)
    $deadline = (Get-Date).AddSeconds(120)
    while ((Get-Date) -lt $deadline) {
        if (Test-Path -LiteralPath $Events) {
            $text = Get-Content -LiteralPath $Events -Raw -ErrorAction SilentlyContinue
            if ($text -and $text -match '"kind":"step\.failed"') { break }
        }
        Start-Sleep -Milliseconds 100
    }
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule(
        $Identity,
        [System.Security.AccessControl.FileSystemRights]::CreateFiles,
        [System.Security.AccessControl.AccessControlType]::Deny
    )
    $current = Get-Acl -LiteralPath $Dir
    $current.RemoveAccessRule($rule) | Out-Null
    Set-Acl -LiteralPath $Dir -AclObject $current
    Set-Content -LiteralPath $Done -Value 'released'
} -ArgumentList $outDir, $retryEvents, $lockDone, $identity
$retry = Invoke-Case -Name $name -Watch $null -Notes 'Denying file creation makes the first write fail; lifting the deny rule on attempt.failed lets a later attempt pass.' -Arguments @(
    'run', '--mission', 'desk_task',
    '--input', (Join-Path $MissionInputs 'basic.json'),
    '--workspace', $workspace,
    '--db', (Join-Path $Root 'retry.db'),
    '--events', $retryEvents
)
Wait-Job -Job $unlockJob -Timeout 30 | Out-Null
$lockState = $unlockJob.State
Remove-Job -Job $unlockJob -Force -ErrorAction SilentlyContinue
if ($lockState -eq 'Failed') { throw 'retry unlock job failed' }
if (-not (Test-Path -LiteralPath $lockDone)) { throw 'retry deny rule was never lifted' }
$summary.Add($retry)
if ($retry.exit_code -ne 0 -or $retry.status -ne 'passed') {
    throw 'retry case did not recover to a truthful pass'
}
if ($retry.first_attempt_failed -ne $true) {
    throw 'retry case did not record a failed first attempt'
}

# ---------------------------------------------------------------- crash
$name = 'crash'
Set-BaseEnvironment $name
$workspace = New-Workspace $name
$crashEvents = Join-Path $EvidenceDir 'crash.events.jsonl'
$crashState = @{ killed = $false }
# Kill the owned server only once the first attempt has started, so the run
# and its attempt exist durably before the model disappears.
$crashWatch = {
    if ($crashState.killed) { return $true }
    if (-not (Test-Path -LiteralPath $crashEvents)) { return $false }
    $text = Get-Content -LiteralPath $crashEvents -Raw -ErrorAction SilentlyContinue
    if (-not $text -or $text -notmatch '"kind":"attempt\.started"') { return $false }
    $pids = @(Get-NewPids)
    if ($pids.Count -eq 0) { return $false }
    foreach ($serverPid in $pids) {
        Stop-Process -Id $serverPid -Force -ErrorAction SilentlyContinue
    }
    $crashState.killed = $true
    return $true
}
$crash = Invoke-Case -Name $name -Watch $crashWatch -Notes 'The owned model process is killed during the first attempt; no false pass and nothing survives.' -Arguments @(
    'run', '--mission', 'desk_task',
    '--input', (Join-Path $MissionInputs 'basic.json'),
    '--workspace', $workspace,
    '--db', (Join-Path $Root 'crash.db'),
    '--events', $crashEvents
)
$summary.Add($crash)
if ($crashState.killed -ne $true) { throw 'crash case never observed an owned model process' }
if ($crash.status -eq 'passed') { throw 'crash case passed after the model was killed' }
if ($crash.owned_servers_gone -ne $true) { throw 'crash case left an owned model process behind' }

# ---------------------------------------------------------------- unavailable
$name = 'unavailable'
Set-BaseEnvironment $name
$workspace = New-Workspace $name
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$listener.Start()
$closedPort = $listener.LocalEndpoint.Port
$listener.Stop()
$env:LFM3_TEXT_URL = "http://127.0.0.1:$closedPort/v1"
$unavailable = Invoke-Case -Name $name -Watch $null -Notes 'An unreachable external endpoint fails instead of starting an owned server.' -Arguments @(
    'run', '--mission', 'desk_task',
    '--input', (Join-Path $MissionInputs 'basic.json'),
    '--workspace', $workspace,
    '--db', (Join-Path $Root 'unavailable.db'),
    '--events', (Join-Path $EvidenceDir 'unavailable.events.jsonl'),
    '--external'
)
$summary.Add($unavailable)
if ($unavailable.exit_code -eq 0 -or $unavailable.status -eq 'passed') { throw 'unavailable case passed' }
if (@($unavailable.leftover_pids).Count -gt 0) { throw 'unavailable case spawned a model process' }

# ---------------------------------------------------------------- timeout
$name = 'timeout'
Set-BaseEnvironment $name
$workspace = New-Workspace $name
$timeout = Invoke-Case -Name $name -Watch $null -Notes 'A one-second deadline cancels the run and terminates the owned server.' -Arguments @(
    'run', '--mission', 'desk_task',
    '--input', (Join-Path $MissionInputs 'basic.json'),
    '--workspace', $workspace,
    '--db', (Join-Path $Root 'timeout.db'),
    '--events', (Join-Path $EvidenceDir 'timeout.events.jsonl'),
    '--timeout', '1'
)
$summary.Add($timeout)
if ($timeout.exit_code -eq 0 -or $timeout.status -eq 'passed') { throw 'timeout case passed' }
if ($timeout.owned_servers_gone -ne $true) { throw 'timeout case left an owned model process behind' }

# ---------------------------------------------------------------- missing
$name = 'missing'
$emptyModels = Join-Path $Root 'empty-models'
if (Test-Path -LiteralPath $emptyModels) { Remove-Item -LiteralPath $emptyModels -Recurse -Force }
New-Item -ItemType Directory -Path $emptyModels -Force | Out-Null
$env:LFM3_DATA_DIR = Join-Path $Root 'state-missing'
$env:LFM3_MODELS_DIR = $emptyModels
$env:LFM3_LLAMA_SERVER = (Resolve-Path -LiteralPath $LlamaServer).Path
$env:LFM3_TEXT_URL = 'http://127.0.0.1:8080/v1'
$workspace = New-Workspace $name
$missing = Invoke-Case -Name $name -Watch $null -Notes 'An empty models directory fails without downloading or spawning a server.' -Arguments @(
    'run', '--mission', 'desk_task',
    '--input', (Join-Path $MissionInputs 'basic.json'),
    '--workspace', $workspace,
    '--db', (Join-Path $Root 'missing.db'),
    '--events', (Join-Path $EvidenceDir 'missing.events.jsonl')
)
$summary.Add($missing)
if ($missing.exit_code -eq 0 -or $missing.status -eq 'passed') { throw 'missing case passed' }
if (@($missing.leftover_pids).Count -gt 0) { throw 'missing case spawned a model process' }
$doctor = Join-Path $EvidenceDir 'missing.doctor.json'
& $Cli doctor --probe-models 2> $null | Set-Content -LiteralPath $doctor -Encoding UTF8
if (-not (Test-Path -LiteralPath $doctor)) { throw 'missing-models doctor report was not written' }

$summary | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $EvidenceDir 'summary.json') -Encoding UTF8
Write-Host ''
Write-Host "Release real-model acceptance passed: $EvidenceDir" -ForegroundColor Green
exit 0
