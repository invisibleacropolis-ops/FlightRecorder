[CmdletBinding()]
param([string]$DistributionPath)

$ErrorActionPreference = 'Stop'
if (-not $DistributionPath) { $DistributionPath = $PSScriptRoot }
$root = (Resolve-Path -LiteralPath $DistributionPath).Path
$sumPath = Join-Path $root 'SHA256SUMS.txt'
if (-not (Test-Path -LiteralPath $sumPath)) { throw 'Distribution is missing SHA256SUMS.txt.' }

$verified = 0
foreach ($line in Get-Content -LiteralPath $sumPath) {
    if (-not $line.Trim()) { continue }
    if ($line -notmatch '^([0-9a-fA-F]{64})  (.+)$') { throw "Invalid checksum line: $line" }
    $relativePath = $Matches[2].Replace('/', [IO.Path]::DirectorySeparatorChar)
    $path = Join-Path $root $relativePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Distribution file is missing: $relativePath" }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash
    if ($actual -ne $Matches[1]) { throw "Checksum mismatch: $relativePath" }
    $verified++
}
if ($verified -eq 0) { throw 'Distribution checksum manifest is empty.' }

foreach ($required in @(
    '.agents\plugins\marketplace.json',
    'plugins\flight-recorder\.codex-plugin\plugin.json',
    'plugins\flight-recorder\.mcp.json',
    'plugins\flight-recorder\hooks\hooks.json',
    'plugins\flight-recorder\skills\flight-recorder\SKILL.md',
    'plugins\flight-recorder\bin\cdxvidext-bridge.exe',
    'plugins\flight-recorder\bin\cdxvidext-desktop.exe',
    'runtime\ffmpeg\8.1.2\bin\ffmpeg.exe',
    'runtime\ffmpeg\8.1.2\bin\ffprobe.exe',
    'runtime\webview2\MicrosoftEdgeWebView2RuntimeInstallerX64.exe',
    'Install-FlightRecorder.ps1',
    'Uninstall-FlightRecorder.ps1',
    'FlightRecorder-Hooks.ps1',
    'Collect-Diagnostics.ps1',
    'BUILDINFO.json',
    'AGENTS.md',
    'README.md'
)) {
    if (-not (Test-Path -LiteralPath (Join-Path $root $required) -PathType Leaf)) { throw "Distribution is missing required file: $required" }
}

$buildInfo = Get-Content -Raw -LiteralPath (Join-Path $root 'BUILDINFO.json') | ConvertFrom-Json
if ($buildInfo.architecture -ne 'x64' -or -not $buildInfo.offline_install) { throw 'BUILDINFO does not identify an offline x64 distribution.' }
$webViewInstaller = Join-Path $root 'runtime\webview2\MicrosoftEdgeWebView2RuntimeInstallerX64.exe'
$signature = Get-AuthenticodeSignature -LiteralPath $webViewInstaller
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft Corporation') {
    throw 'Bundled WebView2 standalone installer does not have a valid Microsoft signature.'
}

Write-Host "Verified $verified files in the offline Flight Recorder distribution." -ForegroundColor Green
