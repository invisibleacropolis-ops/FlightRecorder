[CmdletBinding()]
param([int]$Seconds = 300)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$bridge = Join-Path $repo 'target\release\cdxvidext-bridge.exe'
$desktop = Join-Path $repo 'target\release\cdxvidext-desktop.exe'
if (-not (Test-Path -LiteralPath $bridge) -or -not (Test-Path -LiteralPath $desktop)) {
    throw 'Run scripts\Build-Plugin.ps1 first.'
}
& $bridge open | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Could not open the real Flight Recorder.' }
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
$recordingId = $null
for ($attempt = 0; $attempt -lt 20 -and -not $recordingId; $attempt++) {
    Start-Sleep -Milliseconds 250
    $recordingId = ((& $bridge status | ConvertFrom-Json).data).active_session_id
}
if (-not $recordingId) { throw 'The real WGC capture did not enter Recording.' }
Write-Host "Recording real display and input for $Seconds seconds. Use the machine normally."
Start-Sleep -Seconds $Seconds
$stop = @{ session_id=$session; turn_id=$turn; hook_event_name='Stop'; stop_hook_active=$false; last_assistant_message='Acceptance capture complete.' } | ConvertTo-Json -Compress
$stop | & $bridge hook | Write-Host
Start-Sleep -Seconds 2
$verification = (& $bridge verify $recordingId | ConvertFrom-Json)
if ($LASTEXITCODE -ne 0) { throw 'The real recording verification report failed.' }
if ($verification.duration_ms -lt ($Seconds * 1000)) { throw "Recorded duration was shorter than requested: $($verification.duration_ms) ms" }
$ffprobe = Join-Path $env:LOCALAPPDATA 'CdxVidExt\runtime\ffmpeg\8.1.2\bin\ffprobe.exe'
if (-not (Test-Path -LiteralPath $ffprobe)) {
    $ffprobe = (Get-Command ffprobe -ErrorAction Stop).Source
}
$duration = & $ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1:nokey=1 -- $verification.media_path
if ($LASTEXITCODE -ne 0 -or [double]$duration -lt $Seconds) { throw 'FFprobe did not confirm the requested recording duration.' }
Write-Host "Acceptance session: $recordingId"
Write-Host "Media: $($verification.media_path)"
Write-Host "FFprobe duration: $duration seconds"
