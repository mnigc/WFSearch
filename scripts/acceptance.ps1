<#
  WFSearch acceptance run against a real NTFS volume.

  Run once, from an ELEVATED PowerShell, in the repository root:

      powershell -ExecutionPolicy Bypass -File scripts\acceptance.ps1

  Measures, on this machine's C: volume, the numbers README.md claims:
    0. probe      - wfs-server doctor: every volume ioctl's result
    1. cold start  - process start -> phase=ready (full MFT build), memory/file
    2. query       - P50/P95/P99 of 200 searches (server-side query_ms)
    3. visibility  - create / delete seen by the index (journal poll_ms=100)
    4. warm start  - restart from the snapshot written on shutdown

  Scope: it only creates a throw-away data dir under %TEMP% and launches
  wfs-server.exe in console mode with a generated config. No service is
  installed, %ProgramData% is not touched.

  Output is plain ASCII so it can be pasted into a chat/issue verbatim.
#>

[CmdletBinding()]
param(
    # search rounds per query shape
    [int]$Rounds = 200,
    # seconds to wait for phase=ready on the cold (full build) start
    [int]$ColdTimeoutSec = 900
)

$ErrorActionPreference = 'Stop'
# PowerShell 7.4+ can turn a non-zero native exit code into a terminating error;
# `doctor` exits 1 by design when a volume fails, and its output is the report.
$PSNativeCommandUseErrorActionPreference = $false

# ---------------------------------------------------------------- preconditions

$principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Host 'ERROR: elevation is required (MFT / USN journal access).' -ForegroundColor Red
    Write-Host '       Open PowerShell with "Run as administrator" and re-run this script.'
    exit 1
}

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

Write-Host '[1/6] building the release binary ...'
& cargo build --release -p wfs-server
if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }

$exe = Join-Path $repoRoot 'target\release\wfs-server.exe'
if (-not (Test-Path $exe)) { throw "missing $exe" }

# --------------------------------------------------------------------- helpers

function Get-FreePort {
    $l = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0)
    $l.Start()
    $port = ([System.Net.IPEndPoint]$l.LocalEndpoint).Port
    $l.Stop()
    return $port
}

# Raw loopback HTTP: no proxy, no TLS, no chunking -- mirrors wfs-client and the
# Python/C# samples, so the probe measures the engine and not .NET's networking.
function Get-Http {
    param([int]$Port, [string]$Path)
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $client.Connect('127.0.0.1', $Port)
        $stream = $client.GetStream()
        $bytes = [System.Text.Encoding]::ASCII.GetBytes("GET $Path HTTP/1.0`r`nHost: 127.0.0.1`r`n`r`n")
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush()
        $reader = New-Object System.IO.StreamReader($stream)
        $raw = $reader.ReadToEnd()
        $reader.Close()
    } finally {
        $client.Close()
    }
    $split = $raw.IndexOf("`r`n`r`n")
    if ($split -lt 0) { throw "malformed HTTP reply for $Path" }
    return $raw.Substring($split + 4)
}

function Start-Engine {
    param([string]$ConfigPath)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = "--config `"$ConfigPath`" console"
    $psi.UseShellExecute = $false
    # stdin stays open on purpose: the console REPL exits on EOF.
    $psi.RedirectStandardInput = $true
    # stdout/stderr are captured (not shown live) so a failure report carries
    # the engine's own diagnosis: "volume C: access denied ..." and friends.
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    if ($p.HasExited) { throw "engine exited immediately (code $($p.ExitCode))" }
    $script:engineOut = $p.StandardOutput.ReadToEndAsync()
    $script:engineErr = $p.StandardError.ReadToEndAsync()
    return $p
}

function Stop-Engine {
    param($Process)
    if ($null -eq $Process) { return }
    if (-not $Process.HasExited) {
        $Process.StandardInput.WriteLine('quit')
        $Process.StandardInput.Flush()
        if (-not $Process.WaitForExit(60000)) { $Process.Kill(); throw 'engine did not exit on quit' }
    }
    # drain the readers; both complete once the process is gone
    $script:engineLog = (Get-TaskText $script:engineOut) + (Get-TaskText $script:engineErr)
}

function Get-TaskText {
    param($Task)
    if ($null -eq $Task) { return '' }
    try { return [string]$Task.Result } catch { return '' }
}

# The engine's volume lines and anything it flagged as a warning/error.
function Format-EngineLog {
    param([string]$Log)
    if ([string]::IsNullOrWhiteSpace($Log)) { return '(engine produced no output)' }
    $lines = @($Log -split "`r?`n" | Where-Object { $_ -match 'volume |ERROR|WARN|indexed|snapshot' } | Select-Object -First 40)
    if ($lines.Count -eq 0) { return '(engine produced no volume output)' }
    return ($lines -join "`n    ")
}

function Wait-Status {
    param([int]$Port, [int]$TimeoutSec, $Process)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    while ((Get-Date) -lt $deadline) {
        if ($null -ne $Process -and $Process.HasExited) {
            throw "engine exited early (code $($Process.ExitCode)) - check the console output above"
        }
        $status = $null
        try { $status = Get-Http -Port $Port -Path '/api/v1/status' | ConvertFrom-Json } catch { }
        if ($null -ne $status -and $null -ne $status.volumes) {
            # @() because PowerShell unwraps single-element JSON arrays
            $vols = @($status.volumes)
            $failed = @($vols | Where-Object { $_.phase -eq 'failed' })
            if ($failed.Count -gt 0) {
                throw "volume $($failed[0].drive): phase=failed (see the engine log below)"
            }
            if ($vols[0].phase -eq 'ready') { return $status }
        }
        Start-Sleep -Milliseconds 100
    }
    throw "phase=ready not reached within $TimeoutSec s"
}

function Measure-Query {
    param([int]$Port, [string]$Q, [int]$Count)
    $engine = New-Object System.Collections.Generic.List[double]
    $round = New-Object System.Collections.Generic.List[double]
    $matched = 0
    for ($i = 0; $i -lt $Count; $i++) {
        $escaped = [uri]::EscapeDataString($Q)
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $r = Get-Http -Port $Port -Path "/api/v1/search?q=$escaped&limit=100" | ConvertFrom-Json
        $sw.Stop()
        $engine.Add([double]$r.query_ms)
        $round.Add($sw.Elapsed.TotalMilliseconds)
        $matched = $r.total_matched
    }
    $sorted = $engine | Sort-Object
    $sortedRound = $round | Sort-Object
    return [pscustomobject]@{
        Query        = $Q
        Matched      = $matched
        EngineP50    = $sorted[[int][Math]::Floor($sorted.Count * 0.50)]
        EngineP99    = $sorted[[Math]::Min($sorted.Count - 1, [int][Math]::Floor($sorted.Count * 0.99))]
        RoundTripP50 = $sortedRound[[int][Math]::Floor($sortedRound.Count * 0.50)]
        RoundTripP99 = $sortedRound[[Math]::Min($sortedRound.Count - 1, [int][Math]::Floor($sortedRound.Count * 0.99))]
    }
}

function Format-Mb { param([double]$Bytes) return ('{0:N1} MB' -f ($Bytes / 1MB)) }

# ------------------------------------------------------------------ the run

$port = Get-FreePort
$dataDir = Join-Path $env:TEMP ('wfs-accept-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $dataDir | Out-Null
$configPath = Join-Path $dataDir 'config.toml'
$tomlDir = $dataDir -replace '\\', '\\'
@"
drives = ["C"]
http_port = $port
pipe_name = "\\\\.\\pipe\\wfs-accept-v1"
data_dir = "$tomlDir"
poll_ms = 100
max_limit = 1000
"@ | Set-Content -Path $configPath -Encoding ASCII

Write-Host ("      volume C:, port {0}, data dir {1}" -f $port, $dataDir)

# --- 0. volume probe ----------------------------------------------------------
# One line per ioctl, so a failure below is already explained here.
Write-Host '[2/7] volume probe (wfs-server doctor C) ...'
$doctor = (& $exe doctor C 2>&1 | Out-String).Trim()
Write-Host $doctor

# --- 1. cold start ------------------------------------------------------------
Write-Host '[3/7] cold start (full MFT build) ...'
$proc = Start-Engine -ConfigPath $configPath
$sw = [System.Diagnostics.Stopwatch]::StartNew()
try {
    $status = Wait-Status -Port $port -TimeoutSec $ColdTimeoutSec -Process $proc
} catch {
    Stop-Engine -Process $proc
    Write-Host ''
    Write-Host '--- engine log (cold start) ---'
    Write-Host ('    ' + (Format-EngineLog $script:engineLog))
    throw
} finally {
    $sw.Stop()
}
$coldSec = $sw.Elapsed.TotalSeconds
$files = @($status.volumes)[0].files
$mem = [double]$status.approx_memory_bytes
$bytesPerFile = if ($files -gt 0) { $mem / $files } else { 0 }

# --- 2. query latency ---------------------------------------------------------
Write-Host ("[4/7] query latency, {0} rounds per shape ..." -f $Rounds)
$shapes = @('*.dll', '*.log', 'kernel32')
$latencies = foreach ($q in $shapes) { Measure-Query -Port $port -Q $q -Count $Rounds }

# --- 3. change visibility -----------------------------------------------------
Write-Host '[5/7] change visibility (create + delete) ...'
$stem = 'wfs-accept-' + [guid]::NewGuid().ToString('N').Substring(0, 8)
# the probe must live on the indexed volume (C:), whatever %TEMP% points at
$probeDir = $env:TEMP
if ($probeDir.Substring(0, 2).ToUpper() -ne 'C:') { $probeDir = Join-Path $env:windir 'Temp' }
$probe = Join-Path $probeDir ($stem + '.txt')
$pattern = [uri]::EscapeDataString($stem + '*')
Write-Host ("      probe file: {0}" -f $probe)

Set-Content -Path $probe -Value 'wfs'
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$seen = $false
while ($sw.Elapsed.TotalSeconds -lt 10) {
    $r = Get-Http -Port $port -Path "/api/v1/search?q=$pattern&limit=5" | ConvertFrom-Json
    if ($r.total_matched -ge 1) { $seen = $true; break }
    Start-Sleep -Milliseconds 50
}
$sw.Stop()
if (-not $seen) { throw "created file never showed up in the index (probe: $probe)" }
$createMs = $sw.Elapsed.TotalMilliseconds

Remove-Item -Path $probe -Force
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$gone = $false
while ($sw.Elapsed.TotalSeconds -lt 10) {
    $r = Get-Http -Port $port -Path "/api/v1/search?q=$pattern&limit=5" | ConvertFrom-Json
    if ($r.total_matched -eq 0) { $gone = $true; break }
    Start-Sleep -Milliseconds 50
}
$sw.Stop()
if (-not $gone) { throw 'deleted file stayed in the index' }
$deleteMs = $sw.Elapsed.TotalMilliseconds

# --- 4. warm start ------------------------------------------------------------
Write-Host '[6/7] warm start (snapshot resume) ...'
Stop-Engine -Process $proc
$snap = Join-Path $dataDir 'index.bin'
if (-not (Test-Path $snap)) { throw "no snapshot written at $snap" }
$snapMb = (Get-Item $snap).Length / 1MB

$proc = Start-Engine -ConfigPath $configPath
$sw = [System.Diagnostics.Stopwatch]::StartNew()
try {
    $warm = Wait-Status -Port $port -TimeoutSec 120 -Process $proc
} catch {
    Stop-Engine -Process $proc
    Write-Host ''
    Write-Host '--- engine log (warm start) ---'
    Write-Host ('    ' + (Format-EngineLog $script:engineLog))
    throw
} finally {
    $sw.Stop()
}
$warmSec = $sw.Elapsed.TotalSeconds
$warmFiles = @($warm.volumes)[0].files
Stop-Engine -Process $proc
$coldLog = Format-EngineLog $script:engineLog

# --- report -------------------------------------------------------------------
Write-Host '[7/7] done.'
function Verdict { param([bool]$Ok) if ($Ok) { 'PASS' } else { 'FAIL' } }

Write-Host ''
Write-Host '================= WFSearch acceptance ================='
Write-Host ("host                : {0}" -f $env:COMPUTERNAME)
Write-Host ("binary              : {0}" -f $exe)
Write-Host ("data dir            : {0}" -f $dataDir)
Write-Host ''
Write-Host '[0] volume probe (wfs-server doctor C)'
Write-Host ('    ' + ($doctor -replace "`r?`n", "`n    "))
Write-Host ''
Write-Host '[1] cold start (full MFT build)'
Write-Host ("    indexed files       : {0:N0}" -f $files)
Write-Host ("    time to phase=ready : {0:N1} s" -f $coldSec)
Write-Host ("    approx memory       : {0}  ({1:N0} bytes/file)" -f (Format-Mb $mem), $bytesPerFile)
Write-Host ''
Write-Host ("[2] query latency, {0} rounds per shape (engine query_ms / HTTP round trip)" -f $Rounds)
foreach ($l in $latencies) {
    $line = '    {0,-12} p50 {1,6:N1} ms  p99 {2,6:N1} ms   (round trip p50 {3,6:N1} / p99 {4,6:N1} ms, {5:N0} matches)' -f `
        $l.Query, $l.EngineP50, $l.EngineP99, $l.RoundTripP50, $l.RoundTripP99, $l.Matched
    Write-Host $line
}
Write-Host ''
Write-Host '[3] change visibility (poll_ms=100)'
Write-Host ("    create seen in      : {0:N0} ms" -f $createMs)
Write-Host ("    delete seen in      : {0:N0} ms" -f $deleteMs)
Write-Host ''
Write-Host '[4] warm start (snapshot resume)'
Write-Host ("    snapshot            : index.bin {0:N1} MB" -f $snapMb)
Write-Host ("    time to phase=ready : {0:N1} s   (files {1:N0})" -f $warmSec, $warmFiles)
Write-Host ''
Write-Host '[5] engine log (volume lines)'
Write-Host ('    ' + $coldLog)
Write-Host ''
Write-Host '--- README targets ----------------------------------------------------'
$worstP99 = ($latencies | Measure-Object -Property EngineP99 -Maximum).Maximum
Write-Host ("cold start   < 15 s   : {0}  ({1:N1} s)"   -f (Verdict ($coldSec -lt 15)), $coldSec)
Write-Host ("query P99    < 30 ms  : {0}  ({1:N1} ms)"  -f (Verdict ($worstP99 -lt 30)), $worstP99)
Write-Host ("memory       <= 100 B : {0}  ({1:N0} bytes/file)" -f (Verdict ($bytesPerFile -le 100 -and $bytesPerFile -gt 0)), $bytesPerFile)
Write-Host ("visibility   <= 1 s   : {0}  (create {1:N0} ms / delete {2:N0} ms)" -f (Verdict ([Math]::Max($createMs, $deleteMs) -le 1000)), $createMs, $deleteMs)
Write-Host ("warm restart <= 3 s   : {0}  ({1:N1} s)"  -f (Verdict ($warmSec -lt 3)), $warmSec)
Write-Host '======================================================================='
Write-Host ("artifacts kept in {0} (index.bin, config.toml)" -f $dataDir)
