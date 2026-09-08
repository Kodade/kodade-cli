[CmdletBinding()]
param(
    [string]$InstallDir = $env:KODADE_INSTALL_DIR,
    [string]$Repository = 'Kodade/kodade-cli'
)

$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 5) { throw 'Ködade requires Windows PowerShell 5.1 or newer.' }
if (-not [Environment]::Is64BitOperatingSystem) { throw 'Ködade publishes Windows x64 releases only; 32-bit Windows is unsupported.' }
if ([Environment]::Is64BitProcess -eq $false) { throw 'Run this installer from 64-bit PowerShell.' }
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA 'kodade-cli\bin' }

$target = Join-Path $InstallDir 'kodade-cli.exe'
$assetName = $null
$temp = Join-Path ([IO.Path]::GetTempPath()) ("kodade-install-" + [Guid]::NewGuid())

function Remove-InstallTemp { if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Recurse -Force } }
try {
    New-Item -ItemType Directory -Path $temp | Out-Null
    $release = Invoke-RestMethod -UseBasicParsing -Uri "https://api.github.com/repos/$Repository/releases/latest"
    if ($release.prerelease) { throw 'The latest release is a prerelease; use a stable published release.' }
    $version = ([string]$release.tag_name).TrimStart('v')
    if (-not $version) { throw 'Release metadata has no version tag.' }
    $assetName = "kodade-cli-$version-x86_64-pc-windows-msvc.zip"
    $zipAsset = @($release.assets | Where-Object { $_.name -eq $assetName }) | Select-Object -First 1
    $sumsAsset = @($release.assets | Where-Object { $_.name -eq 'SHA256SUMS' }) | Select-Object -First 1
    if (-not $zipAsset -or -not $sumsAsset) { throw "Release $($release.tag_name) does not publish the required Windows x64 archive and SHA256SUMS." }
    $zipPath = Join-Path $temp $assetName
    $sumsPath = Join-Path $temp 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri $zipAsset.browser_download_url -OutFile $zipPath
    Invoke-WebRequest -UseBasicParsing -Uri $sumsAsset.browser_download_url -OutFile $sumsPath
    $sum = Get-Content -LiteralPath $sumsPath | Where-Object { $_ -match ('^([A-Fa-f0-9]{64})\s+\*?' + [regex]::Escape($assetName) + '$') } | Select-Object -First 1
    if (-not $sum) { throw "SHA256SUMS has no exact entry for $assetName." }
    $expected = ([regex]::Match($sum, '^[A-Fa-f0-9]{64}')).Value.ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $zipPath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { throw "Checksum verification failed for $assetName." }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        if ($archive.Entries.Count -gt 8) { throw 'Release archive has too many entries.' }
        $exe = @($archive.Entries | Where-Object { $_.FullName -eq "kodade-cli-$version-x86_64-pc-windows-msvc/kodade-cli.exe" })
        if ($exe.Count -ne 1 -or $exe[0].Length -le 0 -or $exe[0].Length -gt 100MB) { throw 'Release archive does not contain one bounded kodade-cli.exe.' }
        foreach ($entry in $archive.Entries) {
            if ($entry.FullName -match '(^|/|\\)\.\.($|/|\\)' -or $entry.FullName.StartsWith('/') -or $entry.FullName -match '^[A-Za-z]:') { throw 'Release archive contains an unsafe path.' }
        }
        $candidate = Join-Path $temp 'kodade-cli.exe'
        [IO.Compression.ZipFileExtensions]::ExtractToFile($exe[0], $candidate, $true)
    } finally { $archive.Dispose() }
    $reported = (& $candidate --version 2>&1 | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $reported -ne "kodade-cli $version") { throw "Downloaded executable failed its version probe (expected kodade-cli $version)." }
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $staged = Join-Path $InstallDir (".kodade-cli-" + [Guid]::NewGuid() + '.exe')
    Move-Item -LiteralPath $candidate -Destination $staged
    $backup = "$target.previous"
    try {
        if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Force }
        if (Test-Path -LiteralPath $target) { Move-Item -LiteralPath $target -Destination $backup }
        Move-Item -LiteralPath $staged -Destination $target
        if (Test-Path -LiteralPath $backup) { Remove-Item -LiteralPath $backup -Force }
    } catch {
        if ((-not (Test-Path -LiteralPath $target)) -and (Test-Path -LiteralPath $backup)) { Move-Item -LiteralPath $backup -Destination $target }
        throw "Could not replace $target. It may be running; close it and run 'kodade-cli update' after restarting PowerShell. $($_.Exception.Message)"
    }
    $env:PATH = "$InstallDir;$env:PATH"
    Write-Host "Installed Ködade $version to $target"
    Write-Host "This PowerShell session can now run: kodade-cli --version"
    Write-Host "For future terminals, add '$InstallDir' to your user PATH, then open a new terminal."
} finally { Remove-InstallTemp }
