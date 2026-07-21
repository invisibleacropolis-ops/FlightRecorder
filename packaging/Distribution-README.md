# Flight Recorder offline distribution

This folder contains everything needed to install Flight Recorder on a supported Windows x64 computer that already runs Codex Desktop or Codex CLI. The recipient does not need Rust, Visual Studio, Python, FFmpeg, or an internet connection.

## Preferred Codex Desktop installation

1. Unzip the archive to a normal local folder.
2. Open that folder in Codex Desktop.
3. Send: **Install this plugin for me.**

The included `AGENTS.md` directs Codex to verify every packaged file and use the installer in explicit offline mode.

## Manual PowerShell installation

Run these commands from this folder as the normal signed-in Windows user:

```powershell
.\Verify-Distribution.ps1
.\Install-FlightRecorder.ps1 -BundlePath . -Offline
```

The installer preserves compatible recordings and preferences under `%LOCALAPPDATA%\CdxVidExt`. If Microsoft Edge WebView2 Runtime is absent, it uses the bundled Microsoft-signed x64 standalone installer. It does not add anything to the machine `PATH`.

After installation, review and trust the four Flight Recorder hooks in Codex, restart Codex Desktop, and open a new task.

## Contents

- Local Codex marketplace and complete `flight-recorder` plugin
- Release `cdxvidext-desktop.exe` and `cdxvidext-bridge.exe`
- Pinned FFmpeg 8.1.2 and FFprobe runtime
- Microsoft Edge WebView2 Evergreen Standalone Installer for x64
- Installer, evidence-preserving uninstaller, and redacted diagnostics collector
- Licenses, privacy and terms documents, build metadata, and SHA-256 manifest

Supported systems: Windows 10 version 1903 or newer and Windows 11, x64. The preview binaries are unsigned, so Windows may display a SmartScreen warning. ARM64, macOS, Linux, and audio capture are not supported.
