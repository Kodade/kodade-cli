[CmdletBinding()]
param([string]$Binary = (Join-Path $PSScriptRoot '..\target\debug\kodade-cli.exe'))
$ErrorActionPreference = 'Stop'
if (-not (Test-Path -LiteralPath $Binary)) { throw "missing built Windows binary: $Binary" }
$root = Join-Path ([IO.Path]::GetTempPath()) "kodade-installer-$([guid]::NewGuid())"
$installer = Join-Path $PSScriptRoot '..\install.ps1'
$dest = Join-Path $root 'a path with spaces\bin'
$installed = Join-Path $dest 'kodade-cli.exe'

function Assert-Rejected([scriptblock]$Action, [string]$ExpectedError) {
    $failure = $null
    try { & $Action } catch { $failure = $_ }
    if ($null -eq $failure) { throw "installer unexpectedly accepted $ExpectedError" }
    if ($failure.Exception.Message -notmatch $ExpectedError) {
        throw "expected '$ExpectedError', got: $($failure.Exception.Message)"
    }
}
function Assert-Preserved([string]$ExpectedHash) {
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $installed).Hash -ne $ExpectedHash) {
        throw 'rejected installation changed the existing executable'
    }
    if (Get-ChildItem -LiteralPath $dest -Filter '.kodade-cli-*.exe') {
        throw 'rejected installation leaked staging or backup files'
    }
}
function Repack {
    Compress-Archive -Path $package -DestinationPath $zip -Force
    "$((Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash)  $name" | Set-Content -LiteralPath $sums
}
function Install-Fixture { & $installer -InstallDir $dest -Repository fixture/test }

try {
    $version = (& $Binary --version).Trim().Split(' ')[1]
    $name = "kodade-cli-$version-x86_64-pc-windows-msvc.zip"
    $package = Join-Path $root "kodade-cli-$version-x86_64-pc-windows-msvc"
    New-Item -ItemType Directory -Force -Path $package | Out-Null
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $package 'kodade-cli.exe')
    $zip = Join-Path $root 'release.zip'
    $sums = Join-Path $root 'SHA256SUMS'
    Repack
    $metadata = @{ tag_name = "v$version"; prerelease = $false; assets = @(
        @{ name = $name; browser_download_url = 'https://fixture.test/zip' },
        @{ name = 'SHA256SUMS'; browser_download_url = 'https://fixture.test/sums' }
    ) }
    function Invoke-RestMethod { param($Uri, [int]$TimeoutSec, [switch]$UseBasicParsing) $metadata }
    function Invoke-WebRequest {
        param($Uri, $OutFile, [int]$TimeoutSec, [switch]$UseBasicParsing)
        $source = if ($Uri -eq 'https://fixture.test/zip') { $zip } else { $sums }
        Copy-Item -LiteralPath $source -Destination $OutFile
    }

    Install-Fixture
    if ((& $installed --version).Trim() -ne "kodade-cli $version") {
        throw 'installer did not install the exact executable'
    }
    $expectedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $installed).Hash
    ('0' * 64 + "  $name") | Set-Content -LiteralPath $sums
    Assert-Rejected { Install-Fixture } 'Checksum verification failed'
    Assert-Preserved $expectedHash
    Repack

    # Name both metadata and archive for another release while retaining the
    # original binary. This must reach and fail the executable's version probe.
    $originalPackage = $package
    $originalName = $name
    $package = Join-Path $root 'kodade-cli-999.0.0-x86_64-pc-windows-msvc'
    Move-Item -LiteralPath $originalPackage -Destination $package
    $name = 'kodade-cli-999.0.0-x86_64-pc-windows-msvc.zip'
    $metadata.tag_name = 'v999.0.0'
    $metadata.assets[0].name = $name
    Repack
    Assert-Rejected { Install-Fixture } 'failed its version probe'
    Assert-Preserved $expectedHash
    Move-Item -LiteralPath $package -Destination $originalPackage
    $package = $originalPackage
    $name = $originalName
    $metadata.tag_name = "v$version"
    $metadata.assets[0].name = $name

    'unexpected' | Set-Content -LiteralPath (Join-Path $package 'extra.txt')
    Repack
    Assert-Rejected { Install-Fixture } 'unexpected or unsafe path'
    Assert-Preserved $expectedHash
    Remove-Item -LiteralPath (Join-Path $package 'extra.txt')
    Repack

    $lock = [IO.File]::Open($installed, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
    try { Assert-Rejected { Install-Fixture } 'Could not replace' } finally { $lock.Dispose() }
    Assert-Preserved $expectedHash

    # A normal upgrade replaces old contents only after validation succeeds.
    'old bytes' | Set-Content -LiteralPath $installed
    Install-Fixture
    Assert-Preserved $expectedHash
    Write-Host 'Windows installer passed: initial install, upgrade, spaced path, checksum, executable version, unexpected ZIP entry, locked destination, and preservation checks'
} finally {
    if (Test-Path -LiteralPath $root) { Remove-Item -LiteralPath $root -Recurse -Force }
}
