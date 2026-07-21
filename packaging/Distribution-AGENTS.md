# Flight Recorder offline distribution

This folder is a complete Windows x64 Flight Recorder distribution for Codex Desktop. It is installation media, not a source checkout. Do not build it, download dependencies, or edit its packaged plugin files.

When the user asks to install Flight Recorder from this folder:

1. Use PowerShell as the normal signed-in Windows user. Do not elevate unless the user explicitly asks for a machine-wide WebView2 installation.
2. Run `.\Verify-Distribution.ps1` from this folder and stop if integrity or Microsoft signature verification fails.
3. Run `.\Install-FlightRecorder.ps1 -BundlePath . -Offline` from this folder.
4. Do not use the network as a fallback. The plugin, FFmpeg/FFprobe, WebView2 standalone installer, licenses, hooks, skill, assets, and marketplace are already included.
5. Report the installer result. On success, tell the user to review and trust the four Flight Recorder plugin hooks, restart Codex Desktop, and open a new task.

Preserve existing recordings and preferences under `%LOCALAPPDATA%\CdxVidExt`. Never use `-RemoveEvidence` during installation or upgrade.
