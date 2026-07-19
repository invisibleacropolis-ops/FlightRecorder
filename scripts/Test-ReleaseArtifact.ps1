[CmdletBinding()]
param([string]$BundlePath)

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $BundlePath) {
    $BundlePath = Join-Path $repo 'dist\FlightRecorder-v0.2.0-preview.1-Windows-x64.zip'
}
$bundle = (Resolve-Path -LiteralPath $BundlePath).Path
$sums = Join-Path (Split-Path $bundle -Parent) 'SHA256SUMS.txt'
$expected = (Get-Content -LiteralPath $sums | Where-Object { $_ -match [regex]::Escape((Split-Path $bundle -Leaf)) } | Select-Object -First 1).Split(' ', [StringSplitOptions]::RemoveEmptyEntries)[0]
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $bundle).Hash.ToLowerInvariant()
if (-not $expected -or $actual -ne $expected.ToLowerInvariant()) { throw 'Release ZIP checksum verification failed.' }

$testRoot = Join-Path ([IO.Path]::GetTempPath()) "Flight Recorder Artifact $([guid]::NewGuid())"
New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
try {
    Expand-Archive -LiteralPath $bundle -DestinationPath $testRoot -Force
    $executables = @(
        (Join-Path $testRoot 'plugins\flight-recorder\bin\cdxvidext-bridge.exe'),
        (Join-Path $testRoot 'plugins\flight-recorder\bin\cdxvidext-desktop.exe')
    )
    $dumpbin = Get-ChildItem -LiteralPath 'C:\Program Files (x86)\Microsoft Visual Studio' -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
        Where-Object FullName -match 'Hostx64\\x64\\dumpbin\.exe$' | Select-Object -First 1
    if (-not $dumpbin) { throw 'Visual Studio x64 dumpbin.exe is required for release verification.' }

    foreach ($executable in $executables) {
        $headers = & $dumpbin.FullName /headers $executable 2>&1
        if ($LASTEXITCODE -ne 0 -or ($headers -join "`n") -notmatch 'machine \(x64\)') { throw "$executable is not an x64 PE executable." }
        $imports = & $dumpbin.FullName /dependents $executable 2>&1
        if ($LASTEXITCODE -ne 0) { throw "Could not inspect imports for $executable." }
        if (($imports -join "`n") -match 'VCRUNTIME[^\s]*\.dll') { throw "$executable still imports the dynamic Microsoft C runtime." }
        $signature = Get-AuthenticodeSignature -LiteralPath $executable
        if ($signature.Status -ne 'NotSigned') { throw "$executable must be an explicitly unsigned preview artifact." }
    }

    $scanTargets = @(
        (Join-Path $testRoot 'plugins'),
        (Join-Path $testRoot '.agents'),
        (Join-Path $testRoot 'Install-FlightRecorder.ps1'),
        (Join-Path $testRoot 'Uninstall-FlightRecorder.ps1'),
        (Join-Path $testRoot 'BUILDINFO.json')
    )
    $forbidden = & rg -a -l 'C:\\Users\\|C:\\GITHUB\\|plugins\\cdxvidext' @scanTargets 2>&1
    if ($LASTEXITCODE -eq 0) { $forbidden; throw 'Development-machine path found in the release package.' }
    if ($LASTEXITCODE -ne 1) { throw 'Release package path scan failed.' }

    foreach ($required in @(
        '.agents\plugins\marketplace.json',
        'BUILDINFO.json',
        'Install-FlightRecorder.ps1',
        'Uninstall-FlightRecorder.ps1',
        'runtime\ffmpeg\8.1.2\bin\ffmpeg.exe',
        'runtime\ffmpeg\8.1.2\bin\ffprobe.exe',
        'runtime\ffmpeg\8.1.2\LICENSE.txt',
        'runtime\ffmpeg\8.1.2\README.txt'
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $testRoot $required))) { throw "Release package is missing $required." }
    }
    Write-Host 'Release artifact architecture, CRT, signature, path, checksum, and contents passed.' -ForegroundColor Green
} finally {
    if (Test-Path -LiteralPath $testRoot) {
        $resolved = (Resolve-Path -LiteralPath $testRoot).Path
        if (-not $resolved.StartsWith([IO.Path]::GetTempPath(), [StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe artifact-test cleanup path.' }
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
