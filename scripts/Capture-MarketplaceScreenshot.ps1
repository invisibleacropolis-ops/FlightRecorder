[CmdletBinding()]
param(
    [string]$DesktopExecutable,
    [string]$OutputPath,
    [string]$RestartExecutable
)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $DesktopExecutable) { $DesktopExecutable = Join-Path $repo 'plugins\flight-recorder\bin\cdxvidext-desktop.exe' }
if (-not $OutputPath) { $OutputPath = Join-Path $repo 'plugins\flight-recorder\assets\flight-recorder.png' }
$DesktopExecutable = (Resolve-Path -LiteralPath $DesktopExecutable).Path
$OutputPath = [IO.Path]::GetFullPath($OutputPath)
$isolatedRoot = Join-Path ([IO.Path]::GetTempPath()) "Flight Recorder Screenshot $([guid]::NewGuid())"
$captureProcess = $null

$native = @'
using System;
using System.Runtime.InteropServices;
public static class FlightRecorderScreenshotNative {
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
    [DllImport("user32.dll")]
    public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
}
'@
Add-Type -TypeDefinition $native
Add-Type -AssemblyName System.Drawing.Common

try {
    $running = @(Get-CimInstance Win32_Process -Filter "Name = 'cdxvidext-desktop.exe'")
    foreach ($process in $running) { Stop-Process -Id $process.ProcessId -Force }
    if ($running) { Start-Sleep -Milliseconds 750 }

    New-Item -ItemType Directory -Path $isolatedRoot -Force | Out-Null
    $environment = @{
        LOCALAPPDATA = (Join-Path $isolatedRoot 'Local AppData')
        APPDATA = (Join-Path $isolatedRoot 'Roaming AppData')
    }
    $captureProcess = Start-Process -FilePath $DesktopExecutable -Environment $environment -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds(20)
    do {
        Start-Sleep -Milliseconds 250
        $captureProcess.Refresh()
    } while ($captureProcess.MainWindowHandle -eq [IntPtr]::Zero -and -not $captureProcess.HasExited -and [DateTime]::UtcNow -lt $deadline)
    if ($captureProcess.HasExited -or $captureProcess.MainWindowHandle -eq [IntPtr]::Zero) { throw 'The real Flight Recorder window did not become available.' }
    Start-Sleep -Seconds 2

    $rect = New-Object FlightRecorderScreenshotNative+RECT
    if (-not [FlightRecorderScreenshotNative]::GetWindowRect($captureProcess.MainWindowHandle, [ref]$rect)) { throw 'Could not read the Flight Recorder window bounds.' }
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    $bitmap = New-Object System.Drawing.Bitmap $width, $height
    try {
        $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size, [System.Drawing.CopyPixelOperation]::SourceCopy)
        } finally { $graphics.Dispose() }
        New-Item -ItemType Directory -Path (Split-Path $OutputPath -Parent) -Force | Out-Null
        $bitmap.Save($OutputPath, [System.Drawing.Imaging.ImageFormat]::Png)
    } finally { $bitmap.Dispose() }
    Write-Host "Captured sanitized real first-run UI at $OutputPath" -ForegroundColor Green
} finally {
    if ($captureProcess -and -not $captureProcess.HasExited) { Stop-Process -Id $captureProcess.Id -Force }
    if (Test-Path -LiteralPath $isolatedRoot) {
        $resolved = (Resolve-Path -LiteralPath $isolatedRoot).Path
        if (-not $resolved.StartsWith([IO.Path]::GetTempPath(), [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe screenshot cleanup path.' }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
    if ($RestartExecutable) {
        $restart = (Resolve-Path -LiteralPath $RestartExecutable).Path
        Start-Process -FilePath $restart -WindowStyle Hidden | Out-Null
    }
}
