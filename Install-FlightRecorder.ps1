[CmdletBinding()]
param(
    [string]$Version = '0.2.0-preview.1',
    [string]$BundlePath,
    [switch]$Offline,
    [switch]$SkipWebView2Install
)

$ErrorActionPreference = 'Stop'
$repoSlug = 'invisibleacropolis-ops/FlightRecorder'
$assetName = "FlightRecorder-v$Version-Windows-x64.zip"
$controlRoot = Join-Path $env:LOCALAPPDATA 'CdxVidExt'
$distributionRoot = Join-Path $controlRoot 'distribution\marketplace'
$tempRoot = Join-Path ([IO.Path]::GetTempPath()) "FlightRecorder-Install-$([guid]::NewGuid())"

function Test-WebView2 {
    $id = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    $keys = @(
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\$id",
        "HKCU:\Software\Microsoft\EdgeUpdate\Clients\$id"
    )
    foreach ($key in $keys) {
        $version = (Get-ItemProperty -LiteralPath $key -Name pv -ErrorAction SilentlyContinue).pv
        if ($version -and $version -ne '0.0.0.0') { return $true }
    }
    return $false
}

function Invoke-CodexJson([string[]]$Arguments) {
    $output = & codex @Arguments 2>&1
    if ($LASTEXITCODE -ne 0) { throw ($output -join [Environment]::NewLine) }
    return ($output -join [Environment]::NewLine) | ConvertFrom-Json
}

function Stop-FlightRecorderProcesses {
    Get-Process -Name 'cdxvidext-desktop' -ErrorAction SilentlyContinue | Stop-Process -Force
    foreach ($process in @(Get-Process -Name 'cdxvidext-bridge' -ErrorAction SilentlyContinue)) {
        try {
            $processPath = $process.Path
            if ($processPath -and (
                $processPath.StartsWith($controlRoot, [StringComparison]::OrdinalIgnoreCase) -or
                $processPath -match '(?i)[\\/]\.codex[\\/]plugins[\\/]cache[\\/]flight-recorder[\\/]'
            )) {
                $process | Stop-Process -Force
            }
        } catch {
            Write-Warning "Could not inspect or stop Flight Recorder bridge process $($process.Id): $($_.Exception.Message)"
        }
    }
}

if (-not [Environment]::Is64BitOperatingSystem -or $env:PROCESSOR_ARCHITECTURE -notin @('AMD64', 'x86')) { throw 'Flight Recorder preview requires Windows x64.' }
$os = [Environment]::OSVersion.Version
if ($os.Major -lt 10 -or ($os.Major -eq 10 -and $os.Build -lt 18362)) { throw 'Flight Recorder requires Windows 10 version 1903 or newer.' }
if (-not (Get-Command codex -ErrorAction SilentlyContinue)) { throw 'Codex CLI is required before installing Flight Recorder.' }

New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null
try {
    $bundle = Join-Path $tempRoot $assetName
    if ($BundlePath) {
        $resolvedBundle = (Resolve-Path -LiteralPath $BundlePath).Path
        if ((Get-Item -LiteralPath $resolvedBundle).PSIsContainer) {
            $bundleRoot = $resolvedBundle
        } else {
            $localSums = Join-Path (Split-Path $resolvedBundle -Parent) 'SHA256SUMS.txt'
            if (-not (Test-Path -LiteralPath $localSums)) { throw 'Local release ZIP requires the adjacent SHA256SUMS.txt file.' }
            $expected = (Get-Content -LiteralPath $localSums | Where-Object { $_ -match [regex]::Escape((Split-Path $resolvedBundle -Leaf)) } | Select-Object -First 1).Split(' ', [StringSplitOptions]::RemoveEmptyEntries)[0]
            if (-not $expected) { throw 'Release checksums do not contain the selected local asset.' }
            $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $resolvedBundle).Hash.ToLowerInvariant()
            if ($actual -ne $expected.ToLowerInvariant()) { throw 'Local release asset checksum validation failed.' }
            Copy-Item -LiteralPath $resolvedBundle -Destination $bundle
        }
    } else {
        $base = "https://github.com/$repoSlug/releases/download/v$Version"
        Invoke-WebRequest -Uri "$base/$assetName" -OutFile $bundle
        $sumFile = Join-Path $tempRoot 'SHA256SUMS.txt'
        Invoke-WebRequest -Uri "$base/SHA256SUMS.txt" -OutFile $sumFile
        $expected = (Get-Content -LiteralPath $sumFile | Where-Object { $_ -match [regex]::Escape($assetName) } | Select-Object -First 1).Split(' ', [StringSplitOptions]::RemoveEmptyEntries)[0]
        if (-not $expected) { throw 'Release checksums do not contain the selected asset.' }
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $bundle).Hash.ToLowerInvariant()
        if ($actual -ne $expected.ToLowerInvariant()) { throw 'Release asset checksum validation failed.' }
    }
    if (-not $bundleRoot) {
        $bundleRoot = Join-Path $tempRoot 'bundle'
        Expand-Archive -LiteralPath $bundle -DestinationPath $bundleRoot -Force
    }
    $buildInfo = Get-Content -Raw -LiteralPath (Join-Path $bundleRoot 'BUILDINFO.json') | ConvertFrom-Json
    if ($buildInfo.version -ne $Version -or $buildInfo.architecture -ne 'x64') { throw 'Release bundle metadata does not match the requested version and architecture.' }
    foreach ($executable in @(
        @{ Path = 'plugins\flight-recorder\bin\cdxvidext-bridge.exe'; Hash = 'bridge_sha256' },
        @{ Path = 'plugins\flight-recorder\bin\cdxvidext-desktop.exe'; Hash = 'desktop_sha256' }
    )) {
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $bundleRoot $executable.Path)).Hash.ToLowerInvariant()
        if ($actual -ne $buildInfo.files.($executable.Hash)) { throw "$($executable.Path) checksum validation failed." }
    }
    $hookSupportPath = Join-Path $bundleRoot 'FlightRecorder-Hooks.ps1'
    if (-not (Test-Path -LiteralPath $hookSupportPath)) { throw 'Release bundle is missing Flight Recorder hook installation support.' }
    . $hookSupportPath

    if (-not (Test-WebView2)) {
        if ($SkipWebView2Install) { throw 'WebView2 is missing and automatic installation was disabled.' }
        $bundledWebViewInstaller = Join-Path $bundleRoot 'runtime\webview2\MicrosoftEdgeWebView2RuntimeInstallerX64.exe'
        if (Test-Path -LiteralPath $bundledWebViewInstaller) {
            $webViewInstaller = $bundledWebViewInstaller
            $expectedWebViewHash = $buildInfo.files.webview2_installer_sha256
            if (-not $expectedWebViewHash) { throw 'Bundled WebView2 installer is missing its BUILDINFO checksum.' }
            $actualWebViewHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $webViewInstaller).Hash.ToLowerInvariant()
            if ($actualWebViewHash -ne $expectedWebViewHash.ToLowerInvariant()) { throw 'Bundled WebView2 installer checksum validation failed.' }
        } else {
            if ($Offline) { throw 'WebView2 is missing and the offline bundle does not contain its standalone installer.' }
            $webViewInstaller = Join-Path $tempRoot 'MicrosoftEdgeWebview2Setup.exe'
            Invoke-WebRequest -Uri 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $webViewInstaller
        }
        $signature = Get-AuthenticodeSignature -LiteralPath $webViewInstaller
        if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft Corporation') { throw 'WebView2 bootstrapper signature validation failed.' }
        $process = Start-Process -FilePath $webViewInstaller -ArgumentList '/silent', '/install' -WindowStyle Hidden -Wait -PassThru
        if ($process.ExitCode -ne 0 -or -not (Test-WebView2)) { throw "WebView2 installation failed with exit code $($process.ExitCode)." }
    }

    New-Item -ItemType Directory -Path $controlRoot -Force | Out-Null
    $runtimeSource = Join-Path $bundleRoot "runtime\ffmpeg\$($buildInfo.ffmpeg.version)"
    $runtimeParent = Join-Path $controlRoot 'runtime\ffmpeg'
    $runtimeTarget = Join-Path $runtimeParent $buildInfo.ffmpeg.version
    New-Item -ItemType Directory -Path $runtimeParent -Force | Out-Null
    $runtimeStage = Join-Path $runtimeParent ".$($buildInfo.ffmpeg.version)-$([guid]::NewGuid()).tmp"
    Copy-Item -LiteralPath $runtimeSource -Destination $runtimeStage -Recurse
    foreach ($entry in @('ffmpeg_sha256', 'ffprobe_sha256')) {
        $name = if ($entry -eq 'ffmpeg_sha256') { 'ffmpeg.exe' } else { 'ffprobe.exe' }
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $runtimeStage "bin\$name")).Hash.ToLowerInvariant()
        if ($actual -ne $buildInfo.files.$entry) { throw "$name checksum validation failed." }
    }
    $runtimeBackup = $null
    if (Test-Path -LiteralPath $runtimeTarget) {
        $runtimeBackup = "$runtimeTarget.rollback-$([guid]::NewGuid())"
        Move-Item -LiteralPath $runtimeTarget -Destination $runtimeBackup
    }
    try {
        Move-Item -LiteralPath $runtimeStage -Destination $runtimeTarget
        if ($runtimeBackup) { Remove-Item -LiteralPath $runtimeBackup -Recurse -Force }
    } catch {
        if ($runtimeBackup -and -not (Test-Path -LiteralPath $runtimeTarget)) { Move-Item -LiteralPath $runtimeBackup -Destination $runtimeTarget }
        throw
    }

    $distributionParent = Split-Path $distributionRoot -Parent
    New-Item -ItemType Directory -Path $distributionParent -Force | Out-Null
    $distributionStage = Join-Path $distributionParent ".marketplace-$([guid]::NewGuid()).tmp"
    New-Item -ItemType Directory -Path $distributionStage -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $bundleRoot '.agents') -Destination $distributionStage -Recurse
    Copy-Item -LiteralPath (Join-Path $bundleRoot 'plugins') -Destination $distributionStage -Recurse
    $pluginHookInstallation = Initialize-FlightRecorderPluginHooks `
        -PluginRoot (Join-Path $distributionStage 'plugins\flight-recorder') `
        -Version $buildInfo.version
    $distributionBackup = $null
    if (Test-Path -LiteralPath $distributionRoot) {
        $distributionBackup = "$distributionRoot.rollback-$([guid]::NewGuid())"
        Move-Item -LiteralPath $distributionRoot -Destination $distributionBackup
    }
    try {
        Move-Item -LiteralPath $distributionStage -Destination $distributionRoot
    } catch {
        if ($distributionBackup -and -not (Test-Path -LiteralPath $distributionRoot)) { Move-Item -LiteralPath $distributionBackup -Destination $distributionRoot }
        throw
    }

    $codexHome = Get-FlightRecorderCodexHome
    Uninstall-FlightRecorderHooks -CodexHome $codexHome | Out-Null
    Stop-FlightRecorderProcesses

    $marketplaces = Invoke-CodexJson @('plugin', 'marketplace', 'list', '--json')
    if ($marketplaces.marketplaces.name -contains 'flight-recorder') {
        & codex plugin marketplace remove flight-recorder | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Could not refresh the Flight Recorder marketplace.' }
    }
    Invoke-CodexJson @('plugin', 'marketplace', 'add', $distributionRoot, '--json') | Out-Null

    $installed = Invoke-CodexJson @('plugin', 'list', '--json')
    $previousIds = @($installed.installed | Where-Object { $_.name -in @('cdxvidext', 'flight-recorder') } | ForEach-Object pluginId)
    [ordered]@{
        recorded_at_utc = [DateTime]::UtcNow.ToString('o')
        previous_plugin_ids = $previousIds
        replacement_plugin_id = 'flight-recorder@flight-recorder'
        replacement_hook_install_id = $pluginHookInstallation.InstallId
        evidence_root = $controlRoot
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $controlRoot 'installer-rollback.json') -Encoding utf8
    Stop-FlightRecorderProcesses
    foreach ($previousId in $previousIds) { Invoke-CodexJson @('plugin', 'remove', $previousId, '--json') | Out-Null }
    $pluginCache = Join-Path $codexHome 'plugins\cache\flight-recorder'
    if (Test-Path -LiteralPath $pluginCache) {
        $resolvedCache = (Resolve-Path -LiteralPath $pluginCache).Path
        $expectedCache = [IO.Path]::GetFullPath($pluginCache)
        if ($resolvedCache -ne $expectedCache) { throw "Unsafe Flight Recorder cache path: $resolvedCache" }
        Remove-Item -LiteralPath $resolvedCache -Recurse -Force
    }
    try {
        Invoke-CodexJson @('plugin', 'add', 'flight-recorder@flight-recorder', '--json') | Out-Null
    } catch {
        foreach ($previousId in $previousIds) {
            try { Invoke-CodexJson @('plugin', 'add', $previousId, '--json') | Out-Null } catch { Write-Warning "Could not restore $previousId automatically." }
        }
        if ($distributionBackup -and (Test-Path -LiteralPath $distributionBackup)) {
            if (Test-Path -LiteralPath $distributionRoot) { Remove-Item -LiteralPath $distributionRoot -Recurse -Force }
            Move-Item -LiteralPath $distributionBackup -Destination $distributionRoot
        }
        throw
    }

    if ($distributionBackup -and (Test-Path -LiteralPath $distributionBackup)) { Remove-Item -LiteralPath $distributionBackup -Recurse -Force }

    Stop-FlightRecorderProcesses
    Write-Host 'Flight Recorder installed successfully.' -ForegroundColor Green
    Write-Host 'Next: review and trust the plugin hooks, restart Codex Desktop, and open a new task.'
    Write-Host 'Your existing recordings and preferences were preserved.'
} finally {
    if (Test-Path -LiteralPath $tempRoot) { Remove-Item -LiteralPath $tempRoot -Recurse -Force }
}
