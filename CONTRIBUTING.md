# Contributing

Flight Recorder is a Windows-first Rust and Tauri project. Install the Rust stable MSVC toolchain, Visual Studio C++ build tools, PowerShell, WebView2, and the pinned FFmpeg runtime described in the engineer's manual.

Before proposing a change, run:

```powershell
cargo check --workspace
cargo test --workspace
.\scripts\Build-Plugin.ps1
```

Capture, hook, MCP, installer, and retention changes also require the relevant real acceptance script. Do not replace Windows, FFmpeg, SQLite, DPAPI, WGC, Codex, or filesystem behavior with mocks.

Never include recordings, screenshots containing private information, decrypted input, local databases, secrets, or user-specific absolute paths in an issue or pull request.
