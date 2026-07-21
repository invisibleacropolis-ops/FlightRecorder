[CmdletBinding()]
param([string]$BundlePath)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $BundlePath) {
    $BundlePath = Join-Path $repo 'dist\FlightRecorder-v0.2.0-preview.1-Windows-x64.zip'
}
$bundle = (Resolve-Path -LiteralPath $BundlePath).Path
$testRoot = Join-Path ([IO.Path]::GetTempPath()) "Flight Recorder Portable $([guid]::NewGuid())"
$oldCodexHome = $env:CODEX_HOME
New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
try {
    $package = Join-Path $testRoot 'Release Package With Spaces'
    Expand-Archive -LiteralPath $bundle -DestinationPath $package -Force
    $env:CODEX_HOME = Join-Path $testRoot 'Codex Home With Spaces'
    New-Item -ItemType Directory -Path $env:CODEX_HOME -Force | Out-Null

    & codex plugin marketplace add $package --json | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Codex could not add the release marketplace.' }
    & codex plugin add flight-recorder@flight-recorder --json | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Codex could not install the release plugin.' }
    $installed = & codex plugin list --json | ConvertFrom-Json
    $plugin = $installed.installed | Where-Object pluginId -eq 'flight-recorder@flight-recorder'
    if (-not $plugin -or -not $plugin.enabled) { throw 'Installed release plugin was not enabled.' }

    $cache = Join-Path $env:CODEX_HOME 'plugins\cache\flight-recorder\flight-recorder'
    $bridge = Get-ChildItem -LiteralPath $cache -Recurse -Filter 'cdxvidext-bridge.exe' | Select-Object -First 1
    if (-not $bridge) { throw 'Installed bridge was not found in the Codex cache.' }
    $result = & $bridge.FullName unsupported-portability-probe 2>&1
    if ($LASTEXITCODE -eq 0 -or ($result -join '') -notmatch 'unknown bridge mode') { throw 'Installed bridge did not execute from its cache path.' }

    $hook = Get-Content -Raw -LiteralPath (Join-Path $bridge.Directory.Parent.FullName 'hooks\hooks.json') | ConvertFrom-Json
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
    Write-Host 'Portable marketplace install passed from paths containing spaces.' -ForegroundColor Green
} finally {
    $env:CODEX_HOME = $oldCodexHome
    if (Test-Path -LiteralPath $testRoot) {
        $resolved = (Resolve-Path -LiteralPath $testRoot).Path
        if (-not $resolved.StartsWith([IO.Path]::GetTempPath(), [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe test cleanup path.' }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
exit 0
