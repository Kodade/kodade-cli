$ErrorActionPreference = 'Stop'

function Join-NativeArguments([string[]]$Arguments) {
    (($Arguments | ForEach-Object {
        if ($_ -eq '') { return '""' }
        if ($_ -notmatch '[\s"]') { return $_ }
        '"' + (($_ -replace '(\\*)"', '$1$1\"') -replace '(\\*)$', '$1$1') + '"'
    }) -join ' ')
}

function Invoke-Native(
    [string]$FilePath,
    [string[]]$Arguments,
    [int]$TimeoutSeconds = 20
) {
    $stdout = Join-Path $env:RUNNER_TEMP "kodade-native-$PID-$([guid]::NewGuid()).out"
    $stderr = Join-Path $env:RUNNER_TEMP "kodade-native-$PID-$([guid]::NewGuid()).err"
    try {
        $process = Start-Process -FilePath $FilePath -ArgumentList (Join-NativeArguments $Arguments) `
            -NoNewWindow -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            $process.WaitForExit()
            throw "timed out after ${TimeoutSeconds}s: $FilePath $($Arguments -join ' ')"
        }
        $output = if (Test-Path $stdout) { Get-Content -LiteralPath $stdout -Raw } else { '' }
        $errors = if (Test-Path $stderr) { Get-Content -LiteralPath $stderr -Raw } else { '' }
        if ($process.ExitCode -ne 0) {
            throw "native command failed ($($process.ExitCode)): $FilePath $($Arguments -join ' ')`n$errors$output"
        }
        return $output
    } finally {
        Remove-Item -LiteralPath $stdout, $stderr -Force -ErrorAction SilentlyContinue
    }
}

function Wait-Until([string]$Description, [scriptblock]$Condition, [int]$TimeoutSeconds = 5) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "timed out waiting for $Description"
}

$bin = Join-Path $PSScriptRoot '../target/debug/kodade-cli.exe'
if (-not (Test-Path $bin)) { throw "missing Windows binary: $bin" }

# Keep this smoke independent from a runner's real user configuration.
$env:LOCALAPPDATA = Join-Path $env:RUNNER_TEMP "kodade-cli-windows-smoke-$PID"
$session = "windows-smoke-$PID"
$resultPath = Join-Path $env:RUNNER_TEMP "kodade-conpty-result-$PID.txt"
$hookResultPath = Join-Path $env:RUNNER_TEMP "kodade-hook-result-$PID.txt"
$nodeScript = Join-Path $env:RUNNER_TEMP "kodade-hook-node-$PID.js"

try {
    Write-Host 'starting native ConPTY daemon'
    # `/K` keeps cmd.exe attached to ConPTY so the input assertion exercises a
    # real daemon-owned interactive pane, not only one-shot process output.
    $pane = (Invoke-Native $bin @('--session', $session, 'run', '--', 'cmd.exe', '/K')).Trim()
    if ($pane -notmatch '^\d+$') { throw "run did not return a pane id: $pane" }

    # The result is written by cmd.exe and checked from the filesystem. This
    # cannot pass merely because the terminal echoed the submitted command.
    $expected = 173 * 29
    Invoke-Native $bin @('--session', $session, 'send', $pane, "set /a 173*29 & set /a 173*29 > `"$resultPath`"") | Out-Null
    $lastScreen = ''
    for ($attempt = 0; $attempt -lt 50; $attempt++) {
        if ((Test-Path $resultPath) -and ((Get-Content -LiteralPath $resultPath -Raw).Trim() -eq "$expected")) { break }
        $lastScreen = Invoke-Native $bin @('--session', $session, 'pane', 'read', $pane)
        Start-Sleep -Milliseconds 100
    }
    if (-not ((Test-Path $resultPath) -and ((Get-Content -LiteralPath $resultPath -Raw).Trim() -eq "$expected"))) {
        throw "computed ConPTY result did not appear; pane contents: $lastScreen"
    }
    Wait-Until 'computed ConPTY result rendered in the pane' {
        (Invoke-Native $bin @('--session', $session, 'pane', 'read', $pane)) -match "$expected"
    } 10

    Write-Host 'checking exited ConPTY child'
    $exitedPane = (Invoke-Native $bin @('--session', $session, 'run', '--', 'cmd.exe', '/C', 'echo KODADE_WINDOWS_CHILD_EXIT_OK')).Trim()
    if ($exitedPane -notmatch '^\d+$') { throw "child run did not return a pane id: $exitedPane" }
    $exited = $false
    for ($attempt = 0; $attempt -lt 30; $attempt++) {
        $screen = Invoke-Native $bin @('--session', $session, 'pane', 'read', $exitedPane)
        if ($screen -match 'KODADE_WINDOWS_CHILD_EXIT_OK') {
            $exited = $true
            break
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $exited) { throw 'exited ConPTY child did not retain its output' }

    Write-Host 'checking hook-backed Node identity and replacement refusal'
    if ($null -eq (Get-Command node.exe -ErrorAction SilentlyContinue)) {
        throw 'Windows runner has no node.exe for the hook identity smoke'
    }
@'
const { spawn, spawnSync } = require("child_process");
const report = state => spawnSync(process.env.KODADE_BIN, ["agent", "report", process.env.KODADE_PANE, state, "--source", "kodade:pi", "--native-agent", "pi"]);
// ConPTY can expose cmd.exe for the first scheduler tick after node starts.
// Report after Node is demonstrably the foreground wrapper, so the daemon can
// bind this hook identity to the real process instead of a transient shell.
setTimeout(() => report("working"), 1000);
process.stdin.once("data", () => {
  spawn("cmd.exe", ["/C", "ping 127.0.0.1 -n 10 > NUL"], { stdio: "inherit" });
  process.exit(0);
});
setInterval(() => {}, 1000);
'@ | Set-Content -LiteralPath $nodeScript -NoNewline
    $nodePane = (Invoke-Native $bin @('--session', $session, 'run', '--', 'node.exe', $nodeScript)).Trim()
    if ($nodePane -notmatch '^\d+$') { throw "Node run did not return a pane id: $nodePane" }
    Wait-Until 'hook-backed Node identity' {
        try { (Invoke-Native $bin @('--session', $session, 'agent', 'read', 'Pi') 5).Length -gt 0 } catch { $false }
    }
    Invoke-Native $bin @('--session', $session, 'pane', 'send-keys', $nodePane, 'retire', 'Enter') | Out-Null
    Wait-Until 'replacement process identity retirement' {
        try { Invoke-Native $bin @('--session', $session, 'agent', 'read', 'Pi') 5 | Out-Null; $false } catch { $true }
    } 10
    try {
        Invoke-Native $bin @('--session', $session, 'agent', 'prompt', 'Pi', 'SENTINEL-MUST-NOT-ARRIVE') 5 | Out-Null
        throw 'stale hook identity accepted guarded input after Node replacement'
    } catch {
        if ($_.Exception.Message -match 'stale hook identity accepted') { throw }
    }

    Write-Host 'checking renamed hook endpoint'
    # The pane inherited its private endpoint before the public name changes.
    # A report from that original pane proves its hook still reaches the daemon.
    $renamed = "$session-renamed"
    Invoke-Native $bin @('--session', $session, 'session', 'rename', $renamed) | Out-Null
    Invoke-Native $bin @('--session', $renamed, 'send', $pane, "`"%KODADE_BIN%`" agent report `"%KODADE_PANE%`" working --source windows-smoke && echo hook-ok > `"$hookResultPath`"") | Out-Null
    Wait-Until 'renamed pane hook result' { (Test-Path $hookResultPath) -and ((Get-Content -LiteralPath $hookResultPath -Raw).Trim() -eq 'hook-ok') }
    $session = $renamed

    $sessions = Invoke-Native $bin @('--session', $session, 'session', 'ls', '--json') | ConvertFrom-Json
    if (-not ($sessions | Where-Object { $_.name -eq $session -and $_.alive })) {
        throw 'session ls did not discover the live loopback daemon'
    }

    Write-Host 'stopping native daemon'
    Invoke-Native $bin @('--session', $session, 'kill-session') | Out-Null
    Wait-Until 'session removal' {
        $after = Invoke-Native $bin @('--session', $session, 'session', 'ls', '--json') | ConvertFrom-Json
        -not ($after | Where-Object { $_.name -eq $session })
    }

    Write-Host 'running Windows-to-Unix SSH fixture'
    Invoke-Native (Join-Path $PSHOME 'pwsh.exe') @(
        '-NoProfile', '-File', (Join-Path $PSScriptRoot 'windows-ssh-fixture.ps1'),
        '-WindowsBinary', $bin,
        '-LinuxBinary', (Join-Path $PSScriptRoot '../.windows-ssh-fixture/kodade-cli')
    ) 180 | Write-Host
} finally {
    # The normal path removes this. This protects a failed smoke from leaving
    # cmd.exe or a daemon alive on the hosted runner.
    try { Invoke-Native $bin @('--session', $session, 'kill-session') 5 | Out-Null } catch { }
    Remove-Item -LiteralPath $resultPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $hookResultPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $nodeScript -Force -ErrorAction SilentlyContinue
    Remove-Item -Force -Recurse -ErrorAction SilentlyContinue $env:LOCALAPPDATA
}
