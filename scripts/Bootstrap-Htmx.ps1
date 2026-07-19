[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$destination = Join-Path $PSScriptRoot '..\apps\desktop\ui\htmx.min.js'
Invoke-WebRequest -UseBasicParsing -Uri 'https://unpkg.com/htmx.org@2.0.8/dist/htmx.min.js' -OutFile $destination
$content = Get-Content -Raw -LiteralPath $destination
if ($content.Length -lt 10000 -or $content -notmatch 'htmx') {
    throw 'The downloaded HTMX asset did not pass validation.'
}
Write-Host "Vendored HTMX 2.0.8 to $destination"
