[CmdletBinding()]
param([string]$OutputPath = (Join-Path $env:TEMP "FlightRecorder-Diagnostics-$([DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss')).zip"))

$ErrorActionPreference = 'Stop'
$work = Join-Path $env:TEMP "FlightRecorder-Diagnostics-$([guid]::NewGuid())"
$controlRoot = Join-Path $env:LOCALAPPDATA 'CdxVidExt'
New-Item -ItemType Directory -Path $work -Force | Out-Null
try {
    [ordered]@{
        collected_at_utc = [DateTime]::UtcNow.ToString('o')
        windows = [Environment]::OSVersion.VersionString
        os_64_bit = [Environment]::Is64BitOperatingSystem
        process_architecture = $env:PROCESSOR_ARCHITECTURE
        webview2_machine = (Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -ErrorAction SilentlyContinue).pv
        webview2_user = (Get-ItemProperty 'HKCU:\Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' -ErrorAction SilentlyContinue).pv
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $work 'system.json') -Encoding utf8
    & codex --version 2>&1 | Set-Content -LiteralPath (Join-Path $work 'codex-version.txt') -Encoding utf8
    & codex plugin list --json 2>&1 | Set-Content -LiteralPath (Join-Path $work 'plugins.json') -Encoding utf8
    & codex plugin marketplace list --json 2>&1 | Set-Content -LiteralPath (Join-Path $work 'marketplaces.json') -Encoding utf8
    $codexHome = if ($env:CODEX_HOME) { [IO.Path]::GetFullPath($env:CODEX_HOME) } else { [IO.Path]::GetFullPath((Join-Path $env:USERPROFILE '.codex')) }
    function Get-FlightRecorderHookSummary([string]$Path) {
        if (-not (Test-Path -LiteralPath $Path)) { return @() }
        $document = Get-Content -Raw -LiteralPath $Path | ConvertFrom-Json
        @(
            foreach ($eventProperty in @($document.hooks.PSObject.Properties)) {
                foreach ($group in @($eventProperty.Value)) {
                    foreach ($handler in @($group.hooks)) {
                        $command = [string]$handler.commandWindows
                        if (-not $command) { $command = [string]$handler.command }
                        if ($command -match '(?i)(--flight-recorder-install-id|cdxvidext-bridge(?:\.exe)?["'']?\s+hook)') {
                            [ordered]@{
                                event = $eventProperty.Name
                                install_id = if ($command -match '--flight-recorder-install-id\s+([0-9a-f-]{36})') { $Matches[1] } else { $null }
                                version = if ($command -match '--flight-recorder-version\s+"([^"]+)"') { $Matches[1] } else { $null }
                                plugin_root_relative = ($command -match '%PLUGIN_ROOT%|\$\{PLUGIN_ROOT\}|\$env:PLUGIN_ROOT')
                            }
                        }
                    }
                }
            }
        )
    }
    $userHooksPath = Join-Path $codexHome 'hooks.json'
    $installedPluginHooks = Get-ChildItem -LiteralPath (Join-Path $codexHome 'plugins\cache\flight-recorder') -Recurse -Filter 'hooks.json' -File -ErrorAction SilentlyContinue |
        Where-Object FullName -match '[\\/]hooks[\\/]hooks\.json$' |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1
    $hookReport = [ordered]@{
        user_config_file_exists = (Test-Path -LiteralPath $userHooksPath)
        accidental_user_config_handlers = @(Get-FlightRecorderHookSummary $userHooksPath)
        installed_plugin_hooks_found = ($null -ne $installedPluginHooks)
        plugin_handlers = if ($installedPluginHooks) { @(Get-FlightRecorderHookSummary $installedPluginHooks.FullName) } else { @() }
    }
    $hookReport | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $work 'hook-registration.json') -Encoding utf8
    $runtime = Join-Path $controlRoot 'runtime\ffmpeg\8.1.2\bin'
    foreach ($name in @('ffmpeg.exe', 'ffprobe.exe')) {
        $path = Join-Path $runtime $name
        if (Test-Path -LiteralPath $path) {
            & $path -version 2>&1 | Select-Object -First 4 | Set-Content -LiteralPath (Join-Path $work "$name-version.txt") -Encoding utf8
            Get-FileHash -Algorithm SHA256 -LiteralPath $path | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $work "$name-hash.json") -Encoding utf8
        }
    }
    $logs = Join-Path $controlRoot 'logs'
    if (Test-Path -LiteralPath $logs) { Copy-Item -LiteralPath $logs -Destination (Join-Path $work 'logs') -Recurse }
    Compress-Archive -Path (Join-Path $work '*') -DestinationPath $OutputPath -CompressionLevel Optimal -Force
    Write-Host "Created redacted diagnostics at $OutputPath"
} finally {
    if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
}
