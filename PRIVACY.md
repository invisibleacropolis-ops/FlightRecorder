# Flight Recorder privacy notice

Flight Recorder is local evidence software for Windows computer-use sessions.

When armed, the next matching Codex prompt begins recording the selected display. The resulting MP4 is stored locally and is not encrypted. Anything visibly displayed on that monitor can appear in the recording.

While a flight is active, low-level keyboard and mouse observations apply across the interactive Windows desktop, not only the selected monitor or foreground application. Recognized sensitive text stored in the event database is encrypted with AES-GCM, and its key is protected for the current Windows account with DPAPI. Visible text can still appear in the unencrypted video.

Flight Recorder stores prompt length and SHA-256, not raw prompt text. Evidence is not uploaded by the plugin. A user must explicitly export or share a snapshot before another tool can retrieve it. Retention is off by default, and recordings and snapshots can be deleted in the application.

Control data remains under `%LOCALAPPDATA%\CdxVidExt`. Uninstalling the plugin retains evidence by default. The destructive uninstall option requires separate confirmation.
