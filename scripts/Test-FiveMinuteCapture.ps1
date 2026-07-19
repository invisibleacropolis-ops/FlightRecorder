[CmdletBinding()]
param([int]$Seconds = 300)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$bridge = Join-Path $repo 'target\release\cdxvidext-bridge.exe'
$desktop = Join-Path $repo 'target\release\cdxvidext-desktop.exe'
if (-not (Test-Path -LiteralPath $bridge) -or -not (Test-Path -LiteralPath $desktop)) {
    throw 'Run scripts\Build-Plugin.ps1 first.'
}
Start-Process -FilePath $desktop -WindowStyle Hidden
Read-Host 'In the CdxVidExt window, select the primary monitor and click Arm. Press Enter here when armed'
$session = [guid]::NewGuid().ToString()
$turn = [guid]::NewGuid().ToString()
$prompt = @{ session_id=$session; turn_id=$turn; hook_event_name='UserPromptSubmit'; prompt='CdxVidExt real five-minute acceptance capture' } | ConvertTo-Json -Compress
$timer = [Diagnostics.Stopwatch]::StartNew()
$ack = $prompt | & $bridge hook
$timer.Stop()
if ($LASTEXITCODE -ne 0) { throw 'UserPromptSubmit hook failed.' }
Write-Host "Prompt hook acknowledgement: $($timer.ElapsedMilliseconds) ms — $ack"
if ($timer.ElapsedMilliseconds -gt 500) { Write-Warning 'Acknowledgement exceeded 500 ms.' }
Write-Host "Recording real display and input for $Seconds seconds. Use the machine normally."
Start-Sleep -Seconds $Seconds
$stop = @{ session_id=$session; turn_id=$turn; hook_event_name='Stop'; stop_hook_active=$false; last_assistant_message='Acceptance capture complete.' } | ConvertTo-Json -Compress
$stop | & $bridge hook | Write-Host
Start-Sleep -Seconds 2
$latest = (Get-ChildItem (Join-Path $env:LOCALAPPDATA 'CdxVidExt\sessions') -Directory | Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1).Name
if (-not $latest) { throw 'No real recording session was produced.' }
& $bridge verify $latest
$media = Join-Path $env:LOCALAPPDATA "CdxVidExt\sessions\$latest\capture.mp4"
ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 $media
Write-Host "Acceptance session: $latest"
