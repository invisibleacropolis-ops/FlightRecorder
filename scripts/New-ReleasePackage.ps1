[CmdletBinding()]
param([switch]$SkipBuild, [switch]$SkipPluginValidation, [switch]$KeepStaging)

$ErrorActionPreference = 'Stop'
$version = '0.2.0-preview.1'
$ffmpegVersion = '8.1.2'
$ffmpegSha256 = 'db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec'
$ffmpegUrl = 'https://github.com/GyanD/codexffmpeg/releases/download/8.1.2/ffmpeg-8.1.2-essentials_build.zip'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$dist = Join-Path $repo 'dist'
$downloads = Join-Path $repo 'runtime-downloads'
$staging = Join-Path $repo 'release-staging'
$assetName = "FlightRecorder-v$version-Windows-x64.zip"

New-Item -ItemType Directory -Path $dist, $downloads -Force | Out-Null
if (Test-Path -LiteralPath $staging) {
    $resolved = (Resolve-Path -LiteralPath $staging).Path
    if (-not $resolved.StartsWith($repo, [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe release staging path' }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
New-Item -ItemType Directory -Path $staging -Force | Out-Null

$archive = Join-Path $downloads "ffmpeg-$ffmpegVersion-essentials_build.zip"
if (-not (Test-Path -LiteralPath $archive)) { Invoke-WebRequest -Uri $ffmpegUrl -OutFile $archive }
$actualArchiveHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
if ($actualArchiveHash -ne $ffmpegSha256) { throw "FFmpeg archive hash mismatch: $actualArchiveHash" }

$expanded = Join-Path $staging '_ffmpeg'
Expand-Archive -LiteralPath $archive -DestinationPath $expanded -Force
$ffmpegRoot = Get-ChildItem -LiteralPath $expanded -Directory | Select-Object -First 1
if (-not $ffmpegRoot) { throw 'FFmpeg archive did not contain a root directory' }
if (-not $SkipBuild) {
    $priorPath = $env:PATH
    $env:PATH = "$(Join-Path $ffmpegRoot.FullName 'bin');$priorPath"
    try {
        & (Join-Path $PSScriptRoot 'Build-Plugin.ps1') -SkipValidation:$SkipPluginValidation
    } finally {
        $env:PATH = $priorPath
    }
}
$runtime = Join-Path $staging "runtime\ffmpeg\$ffmpegVersion"
New-Item -ItemType Directory -Path (Join-Path $runtime 'bin') -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $ffmpegRoot.FullName 'bin\ffmpeg.exe') -Destination (Join-Path $runtime 'bin\ffmpeg.exe')
Copy-Item -LiteralPath (Join-Path $ffmpegRoot.FullName 'bin\ffprobe.exe') -Destination (Join-Path $runtime 'bin\ffprobe.exe')
Copy-Item -LiteralPath (Join-Path $ffmpegRoot.FullName 'LICENSE') -Destination (Join-Path $runtime 'LICENSE.txt')
Copy-Item -LiteralPath (Join-Path $ffmpegRoot.FullName 'README.txt') -Destination (Join-Path $runtime 'README.txt')
Remove-Item -LiteralPath $expanded -Recurse -Force

New-Item -ItemType Directory -Path (Join-Path $staging '.agents\plugins'), (Join-Path $staging 'plugins') -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $repo '.agents\plugins\marketplace.json') -Destination (Join-Path $staging '.agents\plugins\marketplace.json')
Copy-Item -LiteralPath (Join-Path $repo 'plugins\flight-recorder') -Destination (Join-Path $staging 'plugins\flight-recorder') -Recurse
Copy-Item -LiteralPath (Join-Path $repo 'Install-FlightRecorder.ps1') -Destination $staging
Copy-Item -LiteralPath (Join-Path $repo 'Uninstall-FlightRecorder.ps1') -Destination $staging
Copy-Item -LiteralPath (Join-Path $repo 'scripts\Collect-Diagnostics.ps1') -Destination $staging
Copy-Item -LiteralPath (Join-Path $repo 'LICENSE') -Destination $staging
Copy-Item -LiteralPath (Join-Path $repo 'PRIVACY.md') -Destination $staging
Copy-Item -LiteralPath (Join-Path $repo 'TERMS.md') -Destination $staging
Copy-Item -LiteralPath (Join-Path $repo 'THIRD_PARTY_NOTICES.md') -Destination $staging

$bridge = Join-Path $staging 'plugins\flight-recorder\bin\cdxvidext-bridge.exe'
$desktop = Join-Path $staging 'plugins\flight-recorder\bin\cdxvidext-desktop.exe'
$buildInfo = [ordered]@{
    version = $version
    architecture = 'x64'
    unsigned_preview = $true
    source_repository = 'https://github.com/invisibleacropolis-ops/FlightRecorder'
    source_commit = (git -C $repo rev-parse HEAD 2>$null)
    built_at_utc = [DateTime]::UtcNow.ToString('o')
    ffmpeg = [ordered]@{ version = $ffmpegVersion; archive_sha256 = $ffmpegSha256; source = $ffmpegUrl }
    files = [ordered]@{
        bridge_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $bridge).Hash.ToLowerInvariant()
        desktop_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $desktop).Hash.ToLowerInvariant()
        ffmpeg_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $runtime 'bin\ffmpeg.exe')).Hash.ToLowerInvariant()
        ffprobe_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $runtime 'bin\ffprobe.exe')).Hash.ToLowerInvariant()
    }
}
$buildInfo | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $staging 'BUILDINFO.json') -Encoding utf8
Copy-Item -LiteralPath (Join-Path $staging 'BUILDINFO.json') -Destination (Join-Path $dist 'BUILDINFO.json') -Force
Copy-Item -LiteralPath (Join-Path $runtime 'LICENSE.txt') -Destination (Join-Path $dist "FFmpeg-$ffmpegVersion-LICENSE.txt") -Force
Copy-Item -LiteralPath (Join-Path $runtime 'README.txt') -Destination (Join-Path $dist "FFmpeg-$ffmpegVersion-README.txt") -Force
Copy-Item -LiteralPath (Join-Path $repo 'THIRD_PARTY_NOTICES.md') -Destination (Join-Path $dist 'THIRD_PARTY_NOTICES.md') -Force

$asset = Join-Path $dist $assetName
if (Test-Path -LiteralPath $asset) { Remove-Item -LiteralPath $asset -Force }
Compress-Archive -Path (Join-Path $staging '*'), (Join-Path $staging '.agents') -DestinationPath $asset -CompressionLevel Optimal
Copy-Item -LiteralPath $bridge -Destination (Join-Path $dist 'cdxvidext-bridge.exe') -Force
Copy-Item -LiteralPath $desktop -Destination (Join-Path $dist 'cdxvidext-desktop.exe') -Force
$releaseFiles = @(
    $asset,
    (Join-Path $dist 'cdxvidext-bridge.exe'),
    (Join-Path $dist 'cdxvidext-desktop.exe'),
    (Join-Path $dist 'BUILDINFO.json'),
    (Join-Path $dist "FFmpeg-$ffmpegVersion-LICENSE.txt"),
    (Join-Path $dist "FFmpeg-$ffmpegVersion-README.txt"),
    (Join-Path $dist 'THIRD_PARTY_NOTICES.md')
)
$sums = foreach ($file in $releaseFiles) { "{0}  {1}" -f (Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash.ToLowerInvariant(), (Split-Path $file -Leaf) }
$sums | Set-Content -LiteralPath (Join-Path $dist 'SHA256SUMS.txt') -Encoding ascii
if (-not $KeepStaging) { Remove-Item -LiteralPath $staging -Recurse -Force }
Write-Host "Created $asset"
