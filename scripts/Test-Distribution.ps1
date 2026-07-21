[CmdletBinding()]
param([string]$DistributionPath)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $DistributionPath) { $DistributionPath = Join-Path $repo 'Distribution' }
$distribution = (Resolve-Path -LiteralPath $DistributionPath).Path

& (Join-Path $distribution 'Verify-Distribution.ps1') -DistributionPath $distribution

$dumpbin = Get-ChildItem -LiteralPath 'C:\Program Files (x86)\Microsoft Visual Studio' -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
    Where-Object FullName -match 'Hostx64\\x64\\dumpbin\.exe$' | Select-Object -First 1
if (-not $dumpbin) { throw 'Visual Studio x64 dumpbin.exe is required for distribution verification.' }
foreach ($executable in @(
    (Join-Path $distribution 'plugins\flight-recorder\bin\cdxvidext-bridge.exe'),
    (Join-Path $distribution 'plugins\flight-recorder\bin\cdxvidext-desktop.exe')
)) {
    $headers = & $dumpbin.FullName /headers $executable 2>&1
    if ($LASTEXITCODE -ne 0 -or ($headers -join "`n") -notmatch 'machine \(x64\)') { throw "$executable is not an x64 PE executable." }
    $imports = & $dumpbin.FullName /dependents $executable 2>&1
    if ($LASTEXITCODE -ne 0) { throw "Could not inspect imports for $executable." }
    if (($imports -join "`n") -match 'VCRUNTIME[^\s]*\.dll') { throw "$executable still imports the dynamic Microsoft C runtime." }
    $signature = Get-AuthenticodeSignature -LiteralPath $executable
    if ($signature.Status -ne 'NotSigned') { throw "$executable must be an explicitly unsigned preview artifact." }
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) "Flight Recorder Distribution $([guid]::NewGuid())"
$oldCodexHome = $env:CODEX_HOME
New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
try {
    $package = Join-Path $testRoot 'Offline Package With Spaces'
    Copy-Item -LiteralPath $distribution -Destination $package -Recurse
    $env:CODEX_HOME = Join-Path $testRoot 'Codex Home With Spaces'
    New-Item -ItemType Directory -Path $env:CODEX_HOME -Force | Out-Null

    & codex plugin marketplace add $package --json | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Codex could not add the distribution marketplace.' }
    & codex plugin add flight-recorder@flight-recorder --json | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Codex could not install the distribution plugin.' }
    $installed = & codex plugin list --json | ConvertFrom-Json
    $plugin = $installed.installed | Where-Object pluginId -eq 'flight-recorder@flight-recorder'
    if (-not $plugin -or -not $plugin.enabled) { throw 'Installed distribution plugin was not enabled.' }

    $cache = Join-Path $env:CODEX_HOME 'plugins\cache\flight-recorder\flight-recorder'
    $bridge = Get-ChildItem -LiteralPath $cache -Recurse -Filter 'cdxvidext-bridge.exe' | Select-Object -First 1
    if (-not $bridge) { throw 'Installed bridge was not found in the isolated Codex cache.' }
    $result = & $bridge.FullName unsupported-portability-probe 2>&1
    if ($LASTEXITCODE -eq 0 -or ($result -join '') -notmatch 'unknown bridge mode') { throw 'Installed bridge did not execute from its cache path.' }

    $hookPath = Join-Path $bridge.Directory.Parent.FullName 'hooks\hooks.json'
    $hook = Get-Content -Raw -LiteralPath $hookPath | ConvertFrom-Json
    $command = $hook.hooks.UserPromptSubmit[0].hooks[0].commandWindows
    if ($command -notmatch '\$env:PLUGIN_ROOT' -or $command -match 'docwh|C:\\GITHUB') { throw 'Installed Windows hook is not PowerShell-safe and plugin-root-relative.' }
    $oldPluginRoot = $env:PLUGIN_ROOT
    try {
        $env:PLUGIN_ROOT = $bridge.Directory.Parent.FullName
        $probeCommand = $command -replace '\s+hook(?:\s+.*)?$', ' unsupported-portability-probe'
        $probe = & pwsh -NoLogo -NoProfile -NonInteractive -Command $probeCommand 2>&1
        if ($LASTEXITCODE -eq 0 -or ($probe -join '') -notmatch 'unknown bridge mode') {
            throw 'Installed Windows hook command did not execute through PowerShell with PLUGIN_ROOT.'
        }
    } finally {
        $env:PLUGIN_ROOT = $oldPluginRoot
    }

    $ffmpeg = Join-Path $package 'runtime\ffmpeg\8.1.2\bin\ffmpeg.exe'
    $ffprobe = Join-Path $package 'runtime\ffmpeg\8.1.2\bin\ffprobe.exe'
    & $ffmpeg -hide_banner -version | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Bundled FFmpeg did not execute.' }
    & $ffprobe -hide_banner -version | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Bundled FFprobe did not execute.' }

    $scanTargets = @(
        (Join-Path $package 'plugins'),
        (Join-Path $package '.agents'),
        (Join-Path $package 'Install-FlightRecorder.ps1'),
        (Join-Path $package 'BUILDINFO.json')
    )
    $forbidden = & rg -a -l 'C:\\Users\\|C:\\GITHUB\\|plugins\\cdxvidext' @scanTargets 2>&1
    if ($LASTEXITCODE -eq 0) { $forbidden; throw 'Development-machine path found in the distribution.' }
    if ($LASTEXITCODE -ne 1) { throw 'Distribution path scan failed.' }

    Write-Host 'Offline distribution installed through Codex from a path containing spaces; plugin, bridge, hooks, FFmpeg, and FFprobe passed.' -ForegroundColor Green
} finally {
    $env:CODEX_HOME = $oldCodexHome
    if (Test-Path -LiteralPath $testRoot) {
        $resolved = (Resolve-Path -LiteralPath $testRoot).Path
        if (-not $resolved.StartsWith([IO.Path]::GetTempPath(), [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe distribution-test cleanup path.' }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
exit 0
