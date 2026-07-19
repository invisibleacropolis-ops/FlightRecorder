# Repository instructions

- Use Windows commands and PowerShell for repository scripts.
- Do not add mock recorder, capture, platform, or filesystem services.
- Tests that cover Windows capture, FFmpeg, Codex installation, or persistence must exercise the real implementation.
- Preserve `%LOCALAPPDATA%\CdxVidExt` compatibility unless a migration is explicitly designed and tested.
- Keep the nine public MCP tools and their response shapes backward compatible.
