param(
    [Parameter(Mandatory = $true)]
    [string]$WindowsBinary,
    [Parameter(Mandatory = $true)]
    [string]$LinuxBinary
)

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
    [int]$TimeoutSeconds = 30
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

# A hosted Windows runner cannot share an Ubuntu job's network namespace. WSL1
# supplies a real Unix kernel ABI on the same localhost instead. This creates a
# one-use Alpine target with an OpenSSH server and a binary built on Ubuntu.
$fixture = Join-Path $env:RUNNER_TEMP "kodade-ssh-fixture-$PID"
$distro = "kodade-ssh-$PID"
$distroRoot = Join-Path $fixture 'distro'
$rootfs = Join-Path $fixture 'alpine.tar.gz'
$clientHome = Join-Path $fixture 'client-home'
$sshProcess = $null
$sshConfig = $null
$sshConfigBackup = $null
$sshConfigExisted = $false
$sshConfigCreated = $false

function Invoke-Wsl([string[]]$Arguments) {
    Invoke-Native 'wsl.exe' (@('-d', $distro, '--') + $Arguments) 45 | Out-Null
}

try {
    if (-not (Test-Path $WindowsBinary)) { throw "missing Windows binary: $WindowsBinary" }
    if (-not (Test-Path $LinuxBinary)) { throw "missing Unix fixture binary: $LinuxBinary" }
    New-Item -ItemType Directory -Force -Path $fixture, $clientHome | Out-Null

    # This release is pinned so the fixture remains repeatable. WSL1 is enabled
    # on GitHub's Windows images and lets the SSH server bind host localhost.
    Write-Host 'downloading Alpine WSL fixture'
    Invoke-WebRequest `
        -Uri 'https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-minirootfs-3.22.1-x86_64.tar.gz' `
        -OutFile $rootfs -TimeoutSec 60
    Write-Host 'importing Alpine as WSL1'
    Invoke-Native 'wsl.exe' @('--import', $distro, $distroRoot, $rootfs, '--version', '1') 60 | Out-Null

    Write-Host 'installing Unix OpenSSH server'
    Invoke-Wsl @('sh', '-lc', 'apk add --no-cache openssh')
    # A freshly imported Alpine rootfs has no host keys. Generate them before
    # the foreground sshd starts so it can bind rather than exiting silently.
    Invoke-Wsl @('ssh-keygen', '-A')
    # WSL imports keep their filesystem opaque to Windows. Copy through the
    # distro's mounted Windows path so the running Unix instance sees the binary.
    if ($LinuxBinary -notmatch '^([A-Za-z]):\\(.*)$') { throw "cannot map Windows fixture path into WSL: $LinuxBinary" }
    $linuxSource = "/mnt/$($matches[1].ToLowerInvariant())/$($matches[2].Replace('\', '/'))"
    $quotedSource = $linuxSource.Replace("'", "'\''")
    Invoke-Wsl @('sh', '-lc', "mkdir -p /root/.local/bin /root/.ssh && cp '$quotedSource' /root/.local/bin/kodade-cli && chmod 700 /root/.local/bin/kodade-cli /root/.ssh")

    $sshDirectory = Join-Path $clientHome '.ssh'
    New-Item -ItemType Directory -Force -Path $sshDirectory | Out-Null
    $key = Join-Path $sshDirectory 'id_ed25519'
    Invoke-Native 'ssh-keygen.exe' @('-q', '-t', 'ed25519', '-N', '', '-f', $key) | Out-Null
    $publicKey = (Get-Content -LiteralPath "$key.pub" -Raw).Trim()
    Invoke-Wsl @('sh', '-lc', "printf '%s\n' '$publicKey' > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys")
    $keyForConfig = $key.Replace('\', '/')
    # Windows OpenSSH discovers its per-user config from the actual runner
    # profile; it ignores HOME and USERPROFILE overrides inherited by a child.
    # Append one PID-scoped host entry and restore the original file in cleanup.
    $sshAlias = "kodade-unix-fixture-$PID"
    $actualSshDirectory = Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)) '.ssh'
    New-Item -ItemType Directory -Force -Path $actualSshDirectory | Out-Null
    $sshConfig = Join-Path $actualSshDirectory 'config'
    $sshConfigBackup = Join-Path $fixture 'runner-ssh-config.backup'
    $sshConfigExisted = Test-Path -LiteralPath $sshConfig
    if ($sshConfigExisted) { Copy-Item -LiteralPath $sshConfig -Destination $sshConfigBackup -Force }
    @"
Host $sshAlias
  HostName 127.0.0.1
  Port 2222
  User root
  IdentityFile $keyForConfig
  IdentitiesOnly yes
  StrictHostKeyChecking no
  UserKnownHostsFile NUL
"@ | Add-Content -LiteralPath $sshConfig -NoNewline
    $sshConfigCreated = $true

    Write-Host 'starting Unix OpenSSH server'
    $sshProcess = Start-Process -FilePath 'wsl.exe' -ArgumentList (Join-NativeArguments @(
        '-d', $distro, '--', '/usr/sbin/sshd', '-D', '-e', '-o', 'Port=2222',
        '-o', 'ListenAddress=127.0.0.1', '-o', 'PermitRootLogin=prohibit-password',
        '-o', 'PasswordAuthentication=no', '-o', 'ChallengeResponseAuthentication=no'
    )) -PassThru
    $ready = $false
    for ($attempt = 0; $attempt -lt 50; $attempt++) {
        if ((Test-NetConnection -ComputerName 127.0.0.1 -Port 2222 -InformationLevel Quiet -WarningAction SilentlyContinue)) {
            $ready = $true
            break
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) { throw 'Unix SSH fixture did not listen on localhost:2222' }

    Write-Host 'running Windows client through the Unix SSH bridge'
    # Both commands create a Windows authenticated loopback bridge, then
    # send daemon protocol through the Windows OpenSSH client into the Unix
    # hidden `bridge` command. The marker originates in a Unix PTY.
    $session = "ssh-fixture-$PID"
    $pane = (Invoke-Native $WindowsBinary @('--remote', $sshAlias, '--session', $session, 'run', '--', 'sh', '-c', 'echo KODADE_WINDOWS_SSH_BRIDGE_OK') 45).Trim()
    if ($pane -notmatch '^\d+$') { throw "remote run did not return a pane id: $pane" }
    $seen = $false
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        $screen = Invoke-Native $WindowsBinary @('--remote', $sshAlias, '--session', $session, 'pane', 'read', $pane) 45
        if ($screen -match 'KODADE_WINDOWS_SSH_BRIDGE_OK') {
            $seen = $true
            break
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $seen) { throw 'Unix PTY output did not return through the Windows SSH bridge' }

    # `agent wait` polls the pane until its hook changes it back to idle.
    # One Windows Tunnel therefore has to accept several sequential daemon
    # connections, rather than only the initial command connection.
    $waitPane = (Invoke-Native $WindowsBinary @('--remote', $sshAlias, '--session', $session, 'run', '--', 'sh', '-c', 'sleep 2; "$KODADE_BIN" agent report "$KODADE_PANE" working --source windows-ssh-fixture; sleep 4; "$KODADE_BIN" agent report "$KODADE_PANE" idle --source windows-ssh-fixture') 45).Trim()
    if ($waitPane -notmatch '^\d+$') { throw "remote wait fixture did not return a pane id: $waitPane" }
    Start-Sleep -Milliseconds 2500
    Invoke-Native $WindowsBinary @('--remote', $sshAlias, '--session', $session, 'agent', 'wait', $waitPane, '--state', 'idle', '--timeout', '10') 45 | Out-Null
    Invoke-Native $WindowsBinary @('--remote', $sshAlias, '--session', $session, 'kill-session') 45 | Out-Null
} finally {
    if ($null -ne $sshProcess -and -not $sshProcess.HasExited) {
        Stop-Process -Id $sshProcess.Id -Force -ErrorAction SilentlyContinue
        $sshProcess.WaitForExit(5000) | Out-Null
    }
    try { Invoke-Native 'wsl.exe' @('--unregister', $distro) 30 | Out-Null } catch { }
    if ($sshConfigCreated) {
        if ($sshConfigExisted) {
            Copy-Item -LiteralPath $sshConfigBackup -Destination $sshConfig -Force
        } else {
            Remove-Item -LiteralPath $sshConfig -Force -ErrorAction SilentlyContinue
        }
    }
    Remove-Item -Force -Recurse -ErrorAction SilentlyContinue $fixture
}
