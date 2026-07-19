---
name: flight-recorder
description: Use when the user wants to open, arm, inspect, query, or retrieve visual evidence from the local Flight Recorder desktop companion.
---

# Flight Recorder

Use the `cdxvidext` MCP tools for recorder state and recorded evidence. The desktop companion owns monitor selection, arming, stopping, deletion, pinning, retention, and frame selection.

## Workflow

1. Call `open_recorder`, then `get_recording_status`.
2. If the recorder is disarmed, ask the user to choose a monitor and arm it in the companion. Do not claim that a prompt was recorded while disarmed.
3. For evidence retrieval, call `list_recording_sessions`, choose a returned session ID, then narrow with `get_session_timeline`. This MCP timeline is the complete raw debug log; the companion's color-coded board is a smaller friendly projection of observed activity.
4. Use `get_frame_at` at action-relevant offsets. When the user refers to shared screenshots or the Shared frames tray, call `list_shared_frames`, then use `get_shared_frame` for a specific share ID or `get_shared_frames` for the complete visible stack.
5. `get_selected_frame` remains a shortcut for the newest frame in the tray. Cite the stable share ID when the evidence came from the tray.
6. Treat OS input events as authoritative. Requested `@oai/sky` actions are best-effort annotations with confidence metadata and do not become standalone visible evidence.

## Privacy and limits

- Never request or expose raw prompt text; the recorder stores only length and SHA-256.
- Keyboard and recognized `type_text` details are encrypted for the current Windows user.
- Session video is local and unencrypted in this feasibility build.
- Cite the session ID and frame offset in conclusions drawn from recordings.
- Do not infer that missing capture evidence proves an action did not occur; report capture gaps explicitly.
