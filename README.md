# Flight Recorder

Flight Recorder creates local, time-addressable video evidence for Codex computer-use sessions on Windows. It records the selected display, correlates real Windows input observations with Codex lifecycle events, and lets a user pause on an exact frame and share that snapshot back through MCP.

![Flight Recorder](artifacts/flight-recorder-branding.png)

## What it provides

- A persistent Windows desktop recorder with Disarmed, Armed, Recording, Finalizing, and Error states.
- Windows Graphics Capture at 30 fps with Low, Medium, and High H.264 profiles.
- 1080p, 2K, and native-resolution recording modes.
- Configurable flight and snapshot storage, retention policies, and automatic cutoff.
- Durable GUI sessions for grouping, switching, renaming, and deleting related flights and snapshots.
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

Rust, Visual Studio, Python, and a system-wide FFmpeg installation are not required when using the installer. The release contains statically linked Rust executables and the pinned FFmpeg 8.1.2 Essentials runtime.

ARM64, macOS, Linux, audio, automatic updates, and Authenticode signing are not currently supported.


## Privacy and local security

First-run consent is required before any UI, bridge, hook, or MCP path can arm the recorder. The notice explains that:

- MP4 video is unencrypted.
- Input observation covers the interactive desktop while recording, not only the selected monitor.
- Evidence stays local unless the user explicitly shares or exports it.
- Retention controls and `Ctrl+Alt+Shift+F12` are available.

The reviewer binds to an ephemeral `127.0.0.1` port. All reviewer routes require a random per-launch HttpOnly session cookie, and state-changing requests require the expected origin. IPC rejects clients from another Windows logon session and limits message size. Programs running as the same Windows user remain inside the local trust boundary.

See [PRIVACY.md](PRIVACY.md), [SECURITY.md](SECURITY.md), and [TERMS.md](TERMS.md).

## Build from source

Building from source is intended for developers who want to modify Flight Recorder or produce their own Windows build. Install these prerequisites first:

- Rust stable with the MSVC target
- Visual Studio C++ Build Tools
- Microsoft Edge WebView2 development components
- PowerShell
- Python for Codex plugin validation
- FFmpeg and FFprobe available on `PATH`

From a PowerShell prompt at the repository root, build the release executables and assemble the Codex plugin:

```powershell
.\scripts\Build-Plugin.ps1
```

Static CRT linking is configured in `.cargo/config.toml`. The build script copies both release executables into `plugins\flight-recorder\bin` and runs the official Codex plugin validator.

To create a distributable bundle from your source build, run:

```powershell
.\scripts\New-ReleasePackage.ps1
```

The packaging script downloads and verifies the pinned FFmpeg archive, includes the required licensing materials, and writes the generated assets under `dist`. See the [Engineering Manual](docs/ENGINEERS_MANUAL.md) for the deeper architecture, development, validation, and troubleshooting reference.

### Create an offline folder for Codex-assisted installation

To build the plugin and assemble a folder that can be handed directly to another Codex Desktop user, place Microsoft's x64 WebView2 Evergreen Standalone Installer at `runtime-downloads\MicrosoftEdgeWebView2RuntimeInstallerX64.exe`, then run:

```powershell
.\scripts\New-Distribution.ps1
.\scripts\Test-Distribution.ps1
```

The generated `Distribution` folder contains the local Codex marketplace, complete plugin and skills, both release executables, pinned FFmpeg/FFprobe, the Microsoft-signed offline WebView2 installer, installation and diagnostics scripts, licenses, build metadata, and a full SHA-256 manifest. The recipient can unzip that folder, open it in Codex Desktop, and prompt: **Install this plugin for me.** Its packaged `AGENTS.md` directs Codex to verify the folder and perform an offline installation.

## Repository layout

- `apps/desktop` — Tauri desktop companion and embedded reviewer
- `apps/bridge` — hooks, CLI, and MCP server
- `crates/core` — capture, storage, privacy, presentation, IPC, and manager logic
- `plugins/flight-recorder` — installable Codex plugin
- `.agents/plugins/marketplace.json` — repository marketplace
- `scripts` — build, packaging, installation, and diagnostic tools
- `docs/ENGINEERS_MANUAL.md` — complete architectural and operational reference

## Licensing

Flight Recorder source is MIT licensed. Release packages aggregate the separate GPLv3 FFmpeg 8.1.2 Essentials executables. Exact source, license, hash, and distributor information are in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) and are included in every package.

## How Codex was used to build this

Flight Recorder was built as a close collaboration between its human designer and Codex Desktop. The project began with a user-written proposal, product priorities, privacy boundaries, and repeated interface direction. Codex helped turn those decisions into feasibility specifications, Rust/Tauri/HTMX implementation work, Codex hooks and MCP integration, real Windows test procedures, troubleshooting, and engineering documentation.

The development process also became a test of the product itself. Codex performed real computer-use tasks while Flight Recorder captured the screen and observed input. The user then reviewed those flights, identified missed actions, recording failures, confusing interface behavior, and evidence-navigation problems, and supplied concrete corrections for the next iteration. In that sense, Flight Recorder became part of its own development feedback loop.

This was not a one-shot generated project. Product judgment and final acceptance remained human-led: the user chose the scope and design, enabled and trusted hooks, inspected recordings, and confirmed interactive behavior. Codex served as the implementation, research, debugging, testing, and documentation collaborator across multiple tasks. The Engineer's Manual preserves that technical context so future Codex tasks, other agents, and new contributors can continue the work without relying on the original conversation history.
