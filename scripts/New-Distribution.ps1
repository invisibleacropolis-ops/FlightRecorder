[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$SkipPluginValidation,
    [string]$WebView2InstallerPath
)

$ErrorActionPreference = 'Stop'
$version = '0.2.0-preview.1'
$ffmpegVersion = '8.1.2'
$ffmpegSha256 = 'db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec'
$ffmpegUrl = 'https://github.com/GyanD/codexffmpeg/releases/download/8.1.2/ffmpeg-8.1.2-essentials_build.zip'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$distribution = Join-Path $repo 'Distribution'
$staging = Join-Path $repo 'distribution-staging'
$downloads = Join-Path $repo 'runtime-downloads'

function Remove-SafeRepositoryDirectory([string]$Path, [string]$ExpectedLeaf) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $resolved = (Resolve-Path -LiteralPath $Path).Path
    $expected = [IO.Path]::GetFullPath((Join-Path $repo $ExpectedLeaf))
    if ($resolved -ne $expected) { throw "Unsafe generated directory path: $resolved" }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}

New-Item -ItemType Directory -Path $downloads -Force | Out-Null
Remove-SafeRepositoryDirectory -Path $staging -ExpectedLeaf 'distribution-staging'
New-Item -ItemType Directory -Path $staging -Force | Out-Null

try {
    $ffmpegArchive = Join-Path $downloads "ffmpeg-$ffmpegVersion-essentials_build.zip"
    if (-not (Test-Path -LiteralPath $ffmpegArchive)) {
        Write-Host "Downloading pinned FFmpeg $ffmpegVersion build dependency..."
        Invoke-WebRequest -Uri $ffmpegUrl -OutFile $ffmpegArchive
    }
    $actualFfmpegArchiveHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $ffmpegArchive).Hash.ToLowerInvariant()
    if ($actualFfmpegArchiveHash -ne $ffmpegSha256) { throw "FFmpeg archive hash mismatch: $actualFfmpegArchiveHash" }

    if ($WebView2InstallerPath) {
        $webViewSource = (Resolve-Path -LiteralPath $WebView2InstallerPath).Path
    } else {
        $webViewSource = Join-Path $downloads 'MicrosoftEdgeWebView2RuntimeInstallerX64.exe'
    }
    if (-not (Test-Path -LiteralPath $webViewSource -PathType Leaf)) {
        throw "Microsoft Edge WebView2 x64 Evergreen Standalone Installer was not found at $webViewSource. Download it on the build machine and pass -WebView2InstallerPath."
    }
    $webViewSignature = Get-AuthenticodeSignature -LiteralPath $webViewSource
    if ($webViewSignature.Status -ne 'Valid' -or $webViewSignature.SignerCertificate.Subject -notmatch 'Microsoft Corporation') {
        throw 'WebView2 standalone installer must have a valid Microsoft Corporation Authenticode signature.'
    }
    $webViewHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $webViewSource).Hash.ToLowerInvariant()

    $expanded = Join-Path $staging '_ffmpeg'
    Expand-Archive -LiteralPath $ffmpegArchive -DestinationPath $expanded -Force
    $ffmpegRoot = Get-ChildItem -LiteralPath $expanded -Directory | Select-Object -First 1
    if (-not $ffmpegRoot) { throw 'FFmpeg archive did not contain a root directory.' }

    if (-not $SkipBuild) {
        $priorPath = $env:PATH
        $env:PATH = "$(Join-Path $ffmpegRoot.FullName 'bin');$priorPath"
        try {
            & (Join-Path $PSScriptRoot 'Build-Plugin.ps1') -SkipValidation:$SkipPluginValidation
        } finally {
            $env:PATH = $priorPath
        }
    }

    foreach ($binary in @('cdxvidext-bridge.exe', 'cdxvidext-desktop.exe')) {
        $binaryPath = Join-Path $repo "plugins\flight-recorder\bin\$binary"
        if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) { throw "Packaged plugin binary is missing: $binary" }
    }

    $ffmpegRuntime = Join-Path $staging "runtime\ffmpeg\$ffmpegVersion"
    New-Item -ItemType Directory -Path (Join-Path $ffmpegRuntime 'bin') -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $ffmpegRoot.FullName 'bin\ffmpeg.exe') -Destination (Join-Path $ffmpegRuntime 'bin\ffmpeg.exe')
    Copy-Item -LiteralPath (Join-Path $ffmpegRoot.FullName 'bin\ffprobe.exe') -Destination (Join-Path $ffmpegRuntime 'bin\ffprobe.exe')
    Copy-Item -LiteralPath (Join-Path $ffmpegRoot.FullName 'LICENSE') -Destination (Join-Path $ffmpegRuntime 'LICENSE.txt')
    Copy-Item -LiteralPath (Join-Path $ffmpegRoot.FullName 'README.txt') -Destination (Join-Path $ffmpegRuntime 'README.txt')
    Remove-Item -LiteralPath $expanded -Recurse -Force

    $webViewRuntime = Join-Path $staging 'runtime\webview2'
    New-Item -ItemType Directory -Path $webViewRuntime -Force | Out-Null
    $webViewTarget = Join-Path $webViewRuntime 'MicrosoftEdgeWebView2RuntimeInstallerX64.exe'
    Copy-Item -LiteralPath $webViewSource -Destination $webViewTarget

    New-Item -ItemType Directory -Path (Join-Path $staging '.agents\plugins'), (Join-Path $staging 'plugins') -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $repo '.agents\plugins\marketplace.json') -Destination (Join-Path $staging '.agents\plugins\marketplace.json')
    Copy-Item -LiteralPath (Join-Path $repo 'plugins\flight-recorder') -Destination (Join-Path $staging 'plugins\flight-recorder') -Recurse
    . (Join-Path $repo 'FlightRecorder-Hooks.ps1')
    $hookInstallation = Initialize-FlightRecorderPluginHooks -PluginRoot (Join-Path $staging 'plugins\flight-recorder') -Version $version

    foreach ($file in @(
        'Install-FlightRecorder.ps1',
        'Uninstall-FlightRecorder.ps1',
        'FlightRecorder-Hooks.ps1',
        'LICENSE',
        'PRIVACY.md',
        'SECURITY.md',
        'TERMS.md',
        'THIRD_PARTY_NOTICES.md'
    )) {
        Copy-Item -LiteralPath (Join-Path $repo $file) -Destination (Join-Path $staging $file)
    }
    Copy-Item -LiteralPath (Join-Path $repo 'scripts\Collect-Diagnostics.ps1') -Destination (Join-Path $staging 'Collect-Diagnostics.ps1')
    Copy-Item -LiteralPath (Join-Path $repo 'scripts\Verify-Distribution.ps1') -Destination (Join-Path $staging 'Verify-Distribution.ps1')
    Copy-Item -LiteralPath (Join-Path $repo 'packaging\Distribution-AGENTS.md') -Destination (Join-Path $staging 'AGENTS.md')
    Copy-Item -LiteralPath (Join-Path $repo 'packaging\Distribution-README.md') -Destination (Join-Path $staging 'README.md')

    $bridge = Join-Path $staging 'plugins\flight-recorder\bin\cdxvidext-bridge.exe'
    $desktop = Join-Path $staging 'plugins\flight-recorder\bin\cdxvidext-desktop.exe'
    $ffmpeg = Join-Path $ffmpegRuntime 'bin\ffmpeg.exe'
    $ffprobe = Join-Path $ffmpegRuntime 'bin\ffprobe.exe'
    $sourceStatus = @(git -C $repo status --porcelain 2>$null)
    $buildInfo = [ordered]@{
        version = $version
        architecture = 'x64'
        distribution_layout_version = 1
        offline_install = $true
        unsigned_preview = $true
        source_repository = 'https://github.com/invisibleacropolis-ops/FlightRecorder'
        source_commit = (git -C $repo rev-parse HEAD 2>$null)
        source_worktree_dirty = ($sourceStatus.Count -gt 0)
        built_at_utc = [DateTime]::UtcNow.ToString('o')
        plugin_hook_install_id = $hookInstallation.InstallId
        ffmpeg = [ordered]@{
            version = $ffmpegVersion
            archive_sha256 = $ffmpegSha256
            source = $ffmpegUrl
        }
        webview2 = [ordered]@{
            installer = 'MicrosoftEdgeWebView2RuntimeInstallerX64.exe'
            installer_product_version = (Get-Item -LiteralPath $webViewSource).VersionInfo.ProductVersion
            signer = $webViewSignature.SignerCertificate.Subject
        }
        files = [ordered]@{
            bridge_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $bridge).Hash.ToLowerInvariant()
            desktop_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $desktop).Hash.ToLowerInvariant()
            ffmpeg_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $ffmpeg).Hash.ToLowerInvariant()
            ffprobe_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $ffprobe).Hash.ToLowerInvariant()
            webview2_installer_sha256 = $webViewHash
        }
    }
    $buildInfo | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $staging 'BUILDINFO.json') -Encoding utf8

    if (-not $SkipPluginValidation) {
        $validator = Join-Path $env:USERPROFILE '.codex\skills\.system\plugin-creator\scripts\validate_plugin.py'
        if (-not (Test-Path -LiteralPath $validator)) { throw "Codex plugin validator not found at $validator" }
        python $validator (Join-Path $staging 'plugins\flight-recorder')
        if ($LASTEXITCODE -ne 0) { throw 'Distribution plugin validation failed.' }
    }

    $sumLines = foreach ($file in Get-ChildItem -LiteralPath $staging -File -Recurse | Sort-Object FullName) {
        if ($file.Name -eq 'SHA256SUMS.txt') { continue }
        $relative = [IO.Path]::GetRelativePath($staging, $file.FullName).Replace('\', '/')
        "{0}  {1}" -f (Get-FileHash -Algorithm SHA256 -LiteralPath $file.FullName).Hash.ToLowerInvariant(), $relative
    }
    $sumLines | Set-Content -LiteralPath (Join-Path $staging 'SHA256SUMS.txt') -Encoding ascii

    & (Join-Path $staging 'Verify-Distribution.ps1') -DistributionPath $staging

    Remove-SafeRepositoryDirectory -Path $distribution -ExpectedLeaf 'Distribution'
    Move-Item -LiteralPath $staging -Destination $distribution
    Write-Host "Created offline Flight Recorder distribution at $distribution" -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $staging) { Remove-SafeRepositoryDirectory -Path $staging -ExpectedLeaf 'distribution-staging' }
}
