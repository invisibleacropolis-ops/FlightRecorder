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
