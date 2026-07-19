[CmdletBinding()]
param(
    [ValidateRange(3, 120)]
    [int]$CutoffSeconds = 6,
    [switch]$KeepEvidence
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$bridge = Join-Path $repo 'target\release\cdxvidext-bridge.exe'
$desktop = Join-Path $repo 'target\release\cdxvidext-desktop.exe'
if (-not (Test-Path -LiteralPath $bridge) -or -not (Test-Path -LiteralPath $desktop)) {
    throw 'Run scripts\Build-Plugin.ps1 first.'
}
if (-not (Get-Command ffprobe -ErrorAction SilentlyContinue)) {
    throw 'FFprobe must be available on PATH.'
}

function Invoke-Bridge {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$BridgeArguments)
    $raw = & $bridge @BridgeArguments
    if ($LASTEXITCODE -ne 0) {
        throw "Bridge command failed: $($BridgeArguments -join ' ')"
    }
    $response = $raw | ConvertFrom-Json
    if ($response.PSObject.Properties.Name -contains 'ok' -and -not $response.ok) {
        throw $response.error
    }
    return $response
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) "CdxVidExt-Preferences-$([guid]::NewGuid())"
$flightRoot = Join-Path $testRoot 'flights'
$snapshotRoot = Join-Path $testRoot 'snapshots'
$backupPath = Join-Path $testRoot 'preferences-backup.json'
$testPreferencesPath = Join-Path $testRoot 'preferences-test.json'
$sessionId = $null
$completed = $false
New-Item -ItemType Directory -Path $flightRoot, $snapshotRoot -Force | Out-Null

try {
    Invoke-Bridge open | Out-Null
    $original = (Invoke-Bridge preferences).data
    $original | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $backupPath -Encoding utf8

    $testPreferences = [ordered]@{
        flight_root = $flightRoot
        snapshot_root = $snapshotRoot
        flight_retention = [ordered]@{ enabled = $false; days = 30; applies_after_utc = $null }
        snapshot_retention = [ordered]@{ enabled = $false; days = 30; applies_after_utc = $null }
        cutoff_seconds = $CutoffSeconds
        quality = 'medium'
        resolution = 'hd1080'
    }
    $testPreferences | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $testPreferencesPath -Encoding utf8
    Invoke-Bridge set-preferences $testPreferencesPath | Out-Null

    Read-Host 'In Flight Recorder, select the real monitor to capture and click Arm. Press Enter here after the status reads Armed'
    $armed = (Invoke-Bridge status).data
    if ($armed.state -ne 'armed' -and $armed.state -ne 'Armed') {
        throw "Recorder must be Armed before the test; current state is $($armed.state)."
    }

    $hookPayload = @{
        session_id = [guid]::NewGuid().ToString()
        turn_id = [guid]::NewGuid().ToString()
        hook_event_name = 'UserPromptSubmit'
        prompt = 'CdxVidExt real Preferences cutoff acceptance capture'
    } | ConvertTo-Json -Compress
    $hookResult = $hookPayload | & $bridge hook
    if ($LASTEXITCODE -ne 0) { throw 'The real UserPromptSubmit hook failed.' }
    $hookAck = $hookResult | ConvertFrom-Json
    if ($hookAck.systemMessage) { throw $hookAck.systemMessage }

    for ($attempt = 0; $attempt -lt 20 -and -not $sessionId; $attempt++) {
        Start-Sleep -Milliseconds 250
        $recording = (Invoke-Bridge status).data
        $sessionId = $recording.active_session_id
    }
    if (-not $sessionId) { throw 'The real WGC capture did not enter Recording.' }
    Write-Host "Recording real session $sessionId until the $CutoffSeconds-second cutoff."

    $deadline = [DateTime]::UtcNow.AddSeconds($CutoffSeconds + 20)
    do {
        Start-Sleep -Milliseconds 500
        $afterCutoff = (Invoke-Bridge status).data
    } while (($afterCutoff.state -ne 'armed' -and $afterCutoff.state -ne 'Armed') -and [DateTime]::UtcNow -lt $deadline)
    if ($afterCutoff.state -ne 'armed' -and $afterCutoff.state -ne 'Armed') {
        throw "Automatic cutoff did not return the recorder to Armed; current state is $($afterCutoff.state)."
    }

    $verification = (& $bridge verify $sessionId) | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Flight verification failed.' }
    if ($verification.automatic_cutoff_events -ne 1) {
        throw "Expected exactly one automatic_cutoff event; found $($verification.automatic_cutoff_events)."
    }
    if ($verification.quality -ne 'medium' -or $verification.resolution_mode -ne 'hd1080') {
        throw 'The flight database did not retain the requested Medium/1080p profile.'
    }
    if (-not (Test-Path -LiteralPath $verification.media_path)) {
        throw "Finalized media is missing: $($verification.media_path)"
    }

    $probe = ffprobe -v error -select_streams v:0 -show_entries stream=width,height,codec_name -show_entries format=duration -of json -- $verification.media_path | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0 -or -not $probe.streams) { throw 'FFprobe could not read the finalized media.' }
    if ($probe.streams[0].width -ne $verification.output_width -or $probe.streams[0].height -ne $verification.output_height) {
        throw 'FFprobe dimensions do not match the indexed recording dimensions.'
    }
    if ([double]$probe.format.duration -le 0) { throw 'Finalized media has no readable duration.' }

    $frame = (Invoke-Bridge frame $sessionId 0).data
    if (-not (Test-Path -LiteralPath $frame.image_path)) { throw 'A readable PNG snapshot was not produced.' }
    $canonicalSnapshotRoot = (Resolve-Path -LiteralPath $snapshotRoot).Path
    $canonicalSnapshot = (Resolve-Path -LiteralPath $frame.image_path).Path
    if (-not $canonicalSnapshot.StartsWith($canonicalSnapshotRoot, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'The extracted PNG was not stored under the selected snapshot root.'
    }

    $completed = $true
    Write-Host "Preferences acceptance passed: $sessionId"
    Write-Host "Encoder: $($verification.encoder_name); output: $($probe.streams[0].width)x$($probe.streams[0].height); duration: $($probe.format.duration)s"
    Write-Host "Snapshot: $($frame.image_path)"
} finally {
    if ($sessionId) {
        try {
            $live = (Invoke-Bridge status).data
            if ($live.active_session_id -eq $sessionId) { Invoke-Bridge stop | Out-Null }
        } catch {
            Write-Warning "Could not finalize the acceptance session during cleanup: $($_.Exception.Message)"
        }
    }
    if (Test-Path -LiteralPath $backupPath) {
        try {
            Invoke-Bridge set-preferences $backupPath | Out-Null
            Write-Host 'Original Preferences restored.'
        } catch {
            Write-Warning "Could not restore original Preferences: $($_.Exception.Message)"
        }
    }
    if ($completed -and -not $KeepEvidence) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    } else {
        Write-Host "Acceptance evidence retained at $testRoot"
    }
}
