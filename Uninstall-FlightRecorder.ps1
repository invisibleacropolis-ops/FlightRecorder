[CmdletBinding()]
param([switch]$RemoveEvidence)

$ErrorActionPreference = 'Stop'
$controlRoot = Join-Path $env:LOCALAPPDATA 'CdxVidExt'
Get-Process -Name 'cdxvidext-desktop' -ErrorAction SilentlyContinue | Stop-Process -Force

$plugins = (& codex plugin list --json 2>$null | ConvertFrom-Json).installed
foreach ($plugin in @($plugins | Where-Object name -eq 'flight-recorder')) {
    & codex plugin remove $plugin.pluginId --json | Out-Null
}
$marketplaces = (& codex plugin marketplace list --json 2>$null | ConvertFrom-Json).marketplaces
if ($marketplaces.name -contains 'flight-recorder') { & codex plugin marketplace remove flight-recorder | Out-Null }

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
