[CmdletBinding()]
param([switch]$SkipTests, [switch]$SkipValidation)
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Push-Location $repo
try {
    & (Join-Path $PSScriptRoot 'Bootstrap-Htmx.ps1')
    if (-not $SkipTests) { cargo test --workspace; if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' } }
    $priorEncodedRustFlags = $env:CARGO_ENCODED_RUSTFLAGS
    $separator = [char]0x1f
    $cargoHome = if ($env:CARGO_HOME) { [IO.Path]::GetFullPath($env:CARGO_HOME) } else { Join-Path $env:USERPROFILE '.cargo' }
    $env:CARGO_ENCODED_RUSTFLAGS = @(
        '-C', 'target-feature=+crt-static',
        "--remap-path-prefix=$repo=flight-recorder-source",
        "--remap-path-prefix=$cargoHome=cargo-home",
        "--remap-path-prefix=$env:USERPROFILE=user-profile"
    ) -join $separator
    try {
        cargo build --workspace --release
        if ($LASTEXITCODE -ne 0) { throw 'release build failed' }
    } finally {
        $env:CARGO_ENCODED_RUSTFLAGS = $priorEncodedRustFlags
    }
    $bin = Join-Path $repo 'plugins\flight-recorder\bin'
    New-Item -ItemType Directory -Path $bin -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $repo 'target\release\cdxvidext-desktop.exe') -Destination $bin -Force
    Copy-Item -LiteralPath (Join-Path $repo 'target\release\cdxvidext-bridge.exe') -Destination $bin -Force
    if (-not $SkipValidation) {
        $validator = Join-Path $env:USERPROFILE '.codex\skills\.system\plugin-creator\scripts\validate_plugin.py'
        if (-not (Test-Path -LiteralPath $validator)) { throw "Codex plugin validator not found at $validator" }
        python $validator (Join-Path $repo 'plugins\flight-recorder')
        if ($LASTEXITCODE -ne 0) { throw 'plugin validation failed' }
    }
    Write-Host "Packaged and validated both Windows executables under $bin"
} finally { Pop-Location }
