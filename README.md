# Flight Recorder

Flight Recorder creates local, time-addressable video evidence for Codex computer-use sessions on Windows. It records the selected display, correlates real Windows input observations with Codex lifecycle events, and lets a user pause on an exact frame and share that snapshot back through MCP.

> **Release status:** `0.2.0-preview.1` is an unsigned Windows x64 preview. The source and packaging are ready for clean-machine acceptance. The public GitHub release remains a draft until that exact package passes the clean Windows test.

![Flight Recorder](artifacts/flight-recorder-branding.png)

## What it provides

- A persistent Windows desktop recorder with Disarmed, Armed, Recording, Finalizing, and Error states.
- Windows Graphics Capture at 30 fps with Low, Medium, and High H.264 profiles.
- 1080p, 2K, and native-resolution recording modes.
- Configurable flight and snapshot storage, retention policies, and automatic cutoff.
- A local evidence reviewer with event navigation and exact-frame PNG extraction.
- A persistent Shared frames tray for handing selected screenshots back to Codex.
- Nine backward-compatible MCP tools under the existing `cdxvidext` namespace.
- Codex prompt/tool/stop lifecycle hooks that never block the main task on recorder failure.

Raw prompt text is not persisted. Prompt length and SHA-256 are retained for correlation. Recognized sensitive input text is AES-GCM encrypted and its key is protected with Windows DPAPI. MP4 screen recordings are local and unencrypted.

## Supported systems

- Windows 10 version 1903 or newer, x64
- Windows 11, x64
- Codex Desktop and Codex CLI
- Microsoft Edge WebView2 Runtime

Rust, Visual Studio, Python, and a system-wide FFmpeg installation are not required for release users. The package contains statically linked Rust executables and the pinned FFmpeg 8.1.2 Essentials runtime.

ARM64, macOS, Linux, audio, automatic updates, and Authenticode signing are not included in this preview.

## Install the preview package

Download these files from the same GitHub release:

- `FlightRecorder-v0.2.0-preview.1-Windows-x64.zip`
- `SHA256SUMS.txt`

Then run PowerShell from the download directory:

```powershell
.\Install-FlightRecorder.ps1 -BundlePath .\FlightRecorder-v0.2.0-preview.1-Windows-x64.zip
```

The installer verifies the release and runtime hashes, installs WebView2 per-user if needed, stages a local Codex marketplace, replaces any installed `cdxvidext` development plugin, and installs `flight-recorder@flight-recorder`. It never changes the machine `PATH` and preserves `%LOCALAPPDATA%\CdxVidExt`.

After installation:

1. Review and trust the Flight Recorder hooks in Codex.
2. Restart Codex Desktop so its MCP configuration reloads.
3. Open a new task.
4. Open Flight Recorder, accept the privacy notice, select a monitor, and arm it.

The two packaged executables are internal components, not separate installers: `cdxvidext-desktop.exe` owns capture and the reviewer, while `cdxvidext-bridge.exe` handles short-lived hooks, CLI commands, and MCP.

## Upgrade and legacy replacement

The installer detects installed plugins whose name is `cdxvidext`, removes their Codex installation after the new package is staged, and installs `flight-recorder`. If installation fails, it attempts to restore the prior plugin IDs. It does not delete the old plugin source folder or any evidence.

The following remain compatible:

- `%LOCALAPPDATA%\CdxVidExt`
- `preferences_v1`
- existing databases, media paths, session IDs, snapshots, and encryption keys
- the `cdxvidext` MCP server namespace and all nine public tools

## Uninstall

```powershell
.\Uninstall-FlightRecorder.ps1
```

This removes the Codex plugin, local marketplace, and managed FFmpeg runtime. Recordings, snapshots, preferences, and keys remain under `%LOCALAPPDATA%\CdxVidExt`.

Permanent evidence removal is deliberately separate:

```powershell
.\Uninstall-FlightRecorder.ps1 -RemoveEvidence
```

The script requires an exact typed confirmation before deleting evidence.

## Privacy and local security

First-run consent is required before any UI, bridge, hook, or MCP path can arm the recorder. The notice explains that:

- MP4 video is unencrypted.
- Input observation covers the interactive desktop while recording, not only the selected monitor.
- Evidence stays local unless the user explicitly shares or exports it.
- Retention controls and `Ctrl+Alt+Shift+F12` are available.

The reviewer binds to an ephemeral `127.0.0.1` port. All reviewer routes require a random per-launch HttpOnly session cookie, and state-changing requests require the expected origin. IPC rejects clients from another Windows logon session and limits message size. Programs running as the same Windows user remain inside the local trust boundary.

See [PRIVACY.md](PRIVACY.md), [SECURITY.md](SECURITY.md), and [TERMS.md](TERMS.md).

## Build from source

Developer prerequisites are the Rust stable MSVC toolchain, Visual Studio C++ build tools, WebView2, PowerShell, Python for official plugin validation, and FFmpeg/FFprobe for the real test suite.

```powershell
cargo check --workspace
cargo test --workspace
.\scripts\Build-Plugin.ps1
```

Static CRT linking is configured in `.cargo/config.toml`. The build script copies both release executables into `plugins\flight-recorder\bin` and runs the official Codex plugin validator.

Create the distributable package:

```powershell
.\scripts\New-ReleasePackage.ps1
```

The script downloads the pinned FFmpeg archive, verifies SHA-256 `db580001caa24ac104c8cb856cd113a87b0a443f7bdf47d8c12b1d740584a2ec`, includes licensing materials, and writes release assets under `dist`.

## Real verification

```powershell
.\scripts\Test-PortablePackage.ps1
.\scripts\Test-ReleaseArtifact.ps1
.\scripts\Test-HookBridge.ps1
.\scripts\Test-Preferences.ps1
.\scripts\Test-FiveMinuteCapture.ps1
```

The suite uses real SQLite files, DPAPI, QPC, FFmpeg, Codex installation, WGC capture, input hooks, and physical media. It does not use a mock recorder or fake platform service. Interactive scripts ask the user to arm a real monitor.

Before publishing the preview, install the exact draft ZIP on a clean standard-user Windows x64 system with no Rust, Visual Studio, Python, or FFmpeg. Verify install, hook trust, consent, recording, cutoff, playback, FFprobe, snapshot sharing, MCP reconnect behavior, uninstall preservation, and reinstall discovery. If that test is unavailable or fails, keep the GitHub release as a draft.

## Repository layout

- `apps/desktop` — Tauri desktop companion and embedded reviewer
- `apps/bridge` — hooks, CLI, and MCP server
- `crates/core` — capture, storage, privacy, presentation, IPC, and manager logic
- `plugins/flight-recorder` — installable Codex plugin
- `.agents/plugins/marketplace.json` — repository marketplace
- `scripts` — real build, package, diagnostic, and acceptance workflows
- `docs/ENGINEERS_MANUAL.md` — complete architectural and operational reference

## Licensing

Flight Recorder source is MIT licensed. Release packages aggregate the separate GPLv3 FFmpeg 8.1.2 Essentials executables. Exact source, license, hash, and distributor information are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) and are included in every package.
