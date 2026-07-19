[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$bridge = Resolve-Path (Join-Path $PSScriptRoot '..\target\release\cdxvidext-bridge.exe')
$payload = @{ session_id=[guid]::NewGuid().ToString(); turn_id=[guid]::NewGuid().ToString(); hook_event_name='UserPromptSubmit'; prompt='hook bridge real recorder probe' } | ConvertTo-Json -Compress
$output = $payload | & $bridge hook
if ($LASTEXITCODE -ne 0) { throw 'Hook bridge returned a failing exit code.' }
$parsed = $output | ConvertFrom-Json
Write-Host "Hook bridge returned valid JSON: $output"
Write-Host 'This test uses the running recorder pipe; it does not simulate a service or Computer Use event.'
