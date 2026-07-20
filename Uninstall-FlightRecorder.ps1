[CmdletBinding()]
param([switch]$RemoveEvidence)

$ErrorActionPreference = 'Stop'
$controlRoot = Join-Path $env:LOCALAPPDATA 'CdxVidExt'
$hookSupportPath = Join-Path $PSScriptRoot 'FlightRecorder-Hooks.ps1'
if (-not (Test-Path -LiteralPath $hookSupportPath)) { throw "Flight Recorder hook removal support is missing: $hookSupportPath" }
. $hookSupportPath
$codexHome = Get-FlightRecorderCodexHome
Uninstall-FlightRecorderHooks -CodexHome $codexHome | Out-Null

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

$plugins = (& codex plugin list --json 2>$null | ConvertFrom-Json).installed
foreach ($plugin in @($plugins | Where-Object name -eq 'flight-recorder')) {
    & codex plugin remove $plugin.pluginId --json | Out-Null
}
$marketplaces = (& codex plugin marketplace list --json 2>$null | ConvertFrom-Json).marketplaces
if ($marketplaces.name -contains 'flight-recorder') { & codex plugin marketplace remove flight-recorder | Out-Null }

$pluginCache = Join-Path $codexHome 'plugins\cache\flight-recorder'
if (Test-Path -LiteralPath $pluginCache) {
    $resolvedCache = (Resolve-Path -LiteralPath $pluginCache).Path
    $expectedCache = [IO.Path]::GetFullPath($pluginCache)
    if ($resolvedCache -ne $expectedCache) { throw "Unsafe uninstall cache path: $resolvedCache" }
    Remove-Item -LiteralPath $resolvedCache -Recurse -Force
}

foreach ($relative in @('distribution', 'runtime\ffmpeg')) {
    $path = Join-Path $controlRoot $relative
    if (Test-Path -LiteralPath $path) {
        $resolved = (Resolve-Path -LiteralPath $path).Path
        if (-not $resolved.StartsWith($controlRoot, [StringComparison]::OrdinalIgnoreCase)) { throw "Unsafe uninstall path: $resolved" }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}

if ($RemoveEvidence) {
    $confirmation = Read-Host 'Type DELETE FLIGHT RECORDER EVIDENCE to permanently remove all recordings, snapshots, preferences, and keys'
    if ($confirmation -ne 'DELETE FLIGHT RECORDER EVIDENCE') { throw 'Evidence deletion was not confirmed.' }
    if (Test-Path -LiteralPath $controlRoot) {
        $resolved = (Resolve-Path -LiteralPath $controlRoot).Path
        $expected = [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'CdxVidExt'))
        if ($resolved -ne $expected) { throw "Unsafe evidence path: $resolved" }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
    Write-Host 'Flight Recorder and all local evidence were removed.'
} else {
    Write-Host "Flight Recorder was removed. Evidence remains at $controlRoot"
}
