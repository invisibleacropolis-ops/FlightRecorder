Set-StrictMode -Version Latest

function Get-FlightRecorderCodexHome {
    [CmdletBinding()]
    param()

    if ($env:CODEX_HOME) {
        return [IO.Path]::GetFullPath($env:CODEX_HOME)
    }
    [IO.Path]::GetFullPath((Join-Path $env:USERPROFILE '.codex'))
}

function Test-FlightRecorderHookHandler {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [System.Collections.IDictionary]$Handler
    )

    $commands = @('command', 'commandWindows', 'command_windows') | ForEach-Object {
        if ($Handler.Contains($_) -and $null -ne $Handler[$_]) { [string]$Handler[$_] }
    }
    $combined = $commands -join "`n"
    if ($combined -match '--flight-recorder-install-id\s+[0-9a-f-]{36}') {
        return $true
    }
    $combined -match 'cdxvidext-bridge(?:\.exe)?["'']?\s+hook' -and
        $combined -match '(?i)(CdxVidExt|flight-recorder|PLUGIN_ROOT)'
}

function Remove-FlightRecorderHookHandlers {
    [CmdletBinding()]
    param([object[]]$Groups)

    $preserved = @()
    foreach ($group in @($Groups)) {
        if ($null -eq $group) {
            continue
        }
        if ($group -isnot [System.Collections.IDictionary] -or -not $group.Contains('hooks')) {
            $preserved += $group
            continue
        }
        $remainingHandlers = @(
            foreach ($handler in @($group['hooks'])) {
                if ($handler -isnot [System.Collections.IDictionary] -or
                    -not (Test-FlightRecorderHookHandler -Handler $handler)) {
                    $handler
                }
            }
        )
        if ($remainingHandlers.Count -gt 0) {
            $group['hooks'] = $remainingHandlers
            $preserved += $group
        }
    }
    $preserved
}

function Write-FlightRecorderHooksDocument {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$HooksPath,

        [Parameter(Mandatory)]
        [System.Collections.IDictionary]$Document
    )

    $parent = Split-Path $HooksPath -Parent
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    $temporaryPath = Join-Path $parent ".hooks-$([guid]::NewGuid()).tmp"
    try {
        $json = $Document | ConvertTo-Json -Depth 100
        [IO.File]::WriteAllText(
            $temporaryPath,
            "$json$([Environment]::NewLine)",
            [Text.UTF8Encoding]::new($false)
        )
        Get-Content -Raw -LiteralPath $temporaryPath | ConvertFrom-Json -AsHashtable | Out-Null
        [IO.File]::Move($temporaryPath, $HooksPath, $true)
    } finally {
        if (Test-Path -LiteralPath $temporaryPath) {
            Remove-Item -LiteralPath $temporaryPath -Force
        }
    }
}

function Initialize-FlightRecorderPluginHooks {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$PluginRoot,

        [Parameter(Mandatory)]
        [string]$Version
    )

    $resolvedPluginRoot = (Resolve-Path -LiteralPath $PluginRoot -ErrorAction Stop).Path
    $hooksPath = Join-Path $resolvedPluginRoot 'hooks\hooks.json'
    $document = Get-Content -Raw -LiteralPath $hooksPath | ConvertFrom-Json -AsHashtable
    if ($document['hooks'] -isnot [System.Collections.IDictionary]) {
        throw "Flight Recorder plugin hooks configuration is invalid: $hooksPath"
    }

    $installId = [guid]::NewGuid()
    $managedHandlers = 0
    foreach ($eventName in @('UserPromptSubmit', 'PreToolUse', 'PostToolUse', 'Stop')) {
        if (-not $document['hooks'].Contains($eventName)) {
            throw "Flight Recorder plugin hook event is missing: $eventName"
        }
        foreach ($group in @($document['hooks'][$eventName])) {
            foreach ($handler in @($group['hooks'])) {
                if ($handler -isnot [System.Collections.IDictionary] -or
                    -not (Test-FlightRecorderHookHandler -Handler $handler)) {
                    continue
                }
                foreach ($commandName in @('command', 'commandWindows', 'command_windows')) {
                    if (-not $handler.Contains($commandName) -or $null -eq $handler[$commandName]) {
                        continue
                    }
                    $baseCommand = [string]$handler[$commandName]
                    $baseCommand = $baseCommand -replace '\s+--flight-recorder-install-id\s+[0-9a-f-]{36}', ''
                    $baseCommand = $baseCommand -replace '\s+--flight-recorder-version\s+"[^"]*"', ''
                    $handler[$commandName] = "$($baseCommand.TrimEnd()) --flight-recorder-install-id $installId --flight-recorder-version `"$Version`""
                }
                $managedHandlers += 1
            }
        }
    }
    if ($managedHandlers -ne 4) {
        throw "Expected exactly four Flight Recorder plugin hook handlers, found $managedHandlers."
    }

    Write-FlightRecorderHooksDocument -HooksPath $hooksPath -Document $document
    [pscustomobject]@{
        PluginRoot = $resolvedPluginRoot
        HooksPath = $hooksPath
        InstallId = $installId.ToString()
        Version = $Version
    }
}

function Uninstall-FlightRecorderHooks {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$CodexHome
    )

    $resolvedCodexHome = [IO.Path]::GetFullPath($CodexHome)
    $hooksPath = Join-Path $resolvedCodexHome 'hooks.json'
    if (-not (Test-Path -LiteralPath $hooksPath)) {
        return [pscustomobject]@{ HooksPath = $hooksPath; Changed = $false; RemovedFile = $false }
    }

    $document = Get-Content -Raw -LiteralPath $hooksPath | ConvertFrom-Json -AsHashtable
    if (-not $document.ContainsKey('hooks')) {
        return [pscustomobject]@{ HooksPath = $hooksPath; Changed = $false; RemovedFile = $false }
    }
    if ($document['hooks'] -isnot [System.Collections.IDictionary]) {
        throw "Codex hooks configuration has a non-object 'hooks' value: $hooksPath"
    }

    $before = $document | ConvertTo-Json -Depth 100 -Compress
    foreach ($eventName in @($document['hooks'].Keys)) {
        $preservedGroups = @(Remove-FlightRecorderHookHandlers -Groups @($document['hooks'][$eventName]))
        if ($preservedGroups.Count -eq 0) {
            $document['hooks'].Remove($eventName)
        } else {
            $document['hooks'][$eventName] = $preservedGroups
        }
    }
    $after = $document | ConvertTo-Json -Depth 100 -Compress
    if ($before -eq $after) {
        return [pscustomobject]@{ HooksPath = $hooksPath; Changed = $false; RemovedFile = $false }
    }

    $hasUserOwnedTopLevelContent = @($document.Keys | Where-Object { $_ -ne 'hooks' }).Count -gt 0
    if ($document['hooks'].Count -eq 0 -and -not $hasUserOwnedTopLevelContent) {
        Remove-Item -LiteralPath $hooksPath -Force
        return [pscustomobject]@{ HooksPath = $hooksPath; Changed = $true; RemovedFile = $true }
    }

    Write-FlightRecorderHooksDocument -HooksPath $hooksPath -Document $document
    [pscustomobject]@{ HooksPath = $hooksPath; Changed = $true; RemovedFile = $false }
}
