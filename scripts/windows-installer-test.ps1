[CmdletBinding()]
param([string]$Binary = (Join-Path $PSScriptRoot '..\target\debug\kodade-cli.exe'))
$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $Binary)) { throw "missing built Windows binary: $Binary" }
$root = Join-Path ([IO.Path]::GetTempPath()) "kodade-installer-$PID"
try {
    $version = (& $Binary --version).Trim().Split(' ')[1]
    $name = "kodade-cli-$version-x86_64-pc-windows-msvc.zip"; $package = Join-Path $root "kodade-cli-$version-x86_64-pc-windows-msvc"
    New-Item -ItemType Directory -Force -Path $package | Out-Null
    Copy-Item $Binary (Join-Path $package 'kodade-cli.exe')
    $zip = Join-Path $root $name; Compress-Archive -Path $package -DestinationPath $zip
    $sums = Join-Path $root SHA256SUMS
    "$(Get-FileHash -Algorithm SHA256 $zip | Select-Object -Expand Hash)  $name" | Set-Content $sums
    $metadata = @{ tag_name = "v$version"; prerelease = $false; assets = @(@{ name = $name; browser_download_url = 'https://fixture.test/zip' }, @{ name = 'SHA256SUMS'; browser_download_url = 'https://fixture.test/sums' }) }
    function Invoke-RestMethod { param($Uri, [int]$TimeoutSec, [switch]$UseBasicParsing) $metadata }
    function Invoke-WebRequest { param($Uri, $OutFile, [int]$TimeoutSec, [switch]$UseBasicParsing) Copy-Item $(if ($Uri -eq 'https://fixture.test/zip') {$zip} else {$sums}) $OutFile }
    $dest = Join-Path $root 'a path with spaces\bin'; & (Join-Path $PSScriptRoot '..\install.ps1') -InstallDir $dest -Repository fixture/test
    $installed = Join-Path $dest 'kodade-cli.exe'
    if ((& $installed --version).Trim() -ne "kodade-cli $version") { throw 'installer did not install the exact executable' }
    'old bytes' | Set-Content $installed; ('0' * 64 + "  $name") | Set-Content $sums
    try { & (Join-Path $PSScriptRoot '..\install.ps1') -InstallDir $dest -Repository fixture/test; throw 'installer accepted a bad checksum' } catch { if ((Get-Content $installed -Raw).Trim() -ne 'old bytes') { throw 'bad checksum replaced existing executable' } }
    Write-Host 'Windows installer fixture passed: real ZIP/version, spaced path, checksum refusal preserves prior bytes'
} finally { if (Test-Path $root) { Remove-Item $root -Recurse -Force } }
