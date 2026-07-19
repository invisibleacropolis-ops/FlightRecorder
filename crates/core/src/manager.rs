use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::json;
use uuid::Uuid;

use crate::capture::{CaptureSession, list_monitors, output_dimensions};
use crate::clock::{qpc_frequency, qpc_now_100ns};
use crate::input::InputObserver;
use crate::model::{
    BridgeRequest, BridgeResponse, HookEvent, McpConnectionStatus, McpHeartbeat, McpInstanceStatus,
    PRIVACY_CONSENT_VERSION, PrivacyConsent, PrivacyConsentStatus, RecorderState, RecorderStatus,
    RuntimeDiagnostics,
};
use crate::parser::parse_sky_actions;
use crate::process::{ffmpeg_command, ffmpeg_path, ffprobe_command, ffprobe_path};
use crate::store::{SessionWriter, Store};

static WINDOW_REVEAL_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Returns the latest request to reveal the persistent desktop companion.
///
/// The manager is shared by the named-pipe server and the Tauri process, so
/// this monotonic value lets an IPC `OpenRecorder` request reach the native
/// window controller without coupling the core crate to Tauri.
pub fn window_reveal_epoch() -> u64 {
    WINDOW_REVEAL_EPOCH.load(Ordering::Acquire)
}

fn request_window_reveal() -> u64 {
    WINDOW_REVEAL_EPOCH
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
}

struct ActiveSession {
    id: String,
    writer: Arc<SessionWriter>,
    capture: CaptureSession,
    input: InputObserver,
    latest_turn_id: String,
    started_at_utc: String,
    origin_100ns: i64,
    cutoff_seconds: Option<u64>,
}

struct RuntimeState {
    state: RecorderState,
    armed: bool,
    monitor_index: Option<usize>,
    active: Option<ActiveSession>,
    last_error: Option<String>,
}

pub struct RecorderManager {
    store: Arc<Store>,
    runtime: Mutex<RuntimeState>,
    mcp_instances: Mutex<HashMap<String, McpPresence>>,
}

struct McpPresence {
    heartbeat: McpHeartbeat,
    last_seen: Instant,
    last_seen_utc: String,
}

impl RecorderManager {
    pub fn open_default() -> Result<Arc<Self>> {
        Self::open_store(Store::open_default()?)
    }

    pub fn open(root: PathBuf) -> Result<Arc<Self>> {
        Self::open_store(Store::open(root)?)
    }

    fn open_store(store: Arc<Store>) -> Result<Arc<Self>> {
        let _ = store.recover_stale_sessions()?;
        let _ = store.purge_expired_preferences()?;
        let manager = Arc::new(Self {
            store,
            runtime: Mutex::new(RuntimeState {
                state: RecorderState::Disarmed,
                armed: false,
                monitor_index: None,
                active: None,
                last_error: None,
            }),
            mcp_instances: Mutex::new(HashMap::new()),
        });
        manager.start_maintenance()?;
        Ok(manager)
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn record_mcp_heartbeat(&self, heartbeat: McpHeartbeat) {
        self.mcp_instances.lock().insert(
            heartbeat.instance_id.clone(),
            McpPresence {
                heartbeat,
                last_seen: Instant::now(),
                last_seen_utc: Utc::now().to_rfc3339(),
            },
        );
    }

    pub fn disconnect_mcp(&self, instance_id: &str) {
        self.mcp_instances.lock().remove(instance_id);
    }

    pub fn mcp_status(&self) -> McpConnectionStatus {
        let mut instances = self.mcp_instances.lock();
        instances.retain(|_, presence| presence.last_seen.elapsed() < Duration::from_secs(5));
        let primary = instances
            .values()
            .max_by_key(|presence| presence.last_seen)
            .map(|presence| McpInstanceStatus {
                instance_id: presence.heartbeat.instance_id.clone(),
                pid: presence.heartbeat.pid,
                version: presence.heartbeat.version.clone(),
                executable_path: presence.heartbeat.executable_path.clone(),
                started_at_utc: presence.heartbeat.started_at_utc.clone(),
                last_seen_utc: presence.last_seen_utc.clone(),
            });
        McpConnectionStatus {
            connected: primary.is_some(),
            active_instances: instances.len(),
            primary,
        }
    }

    fn start_maintenance(self: &Arc<Self>) -> Result<()> {
        let manager = Arc::downgrade(self);
        thread::Builder::new()
            .name("cdx-maintenance".into())
            .spawn(move || {
                let mut last_purge = Instant::now();
                loop {
                    let Some(manager) = manager.upgrade() else {
                        return;
                    };
                    if let Err(error) = manager.check_automatic_cutoff() {
                        let mut runtime = manager.runtime.lock();
                        runtime.last_error = Some(format!("automatic cutoff failed: {error:#}"));
                    }
                    if last_purge.elapsed() >= Duration::from_secs(60 * 60) {
                        if let Err(error) = manager.store.purge_expired_preferences() {
                            let mut runtime = manager.runtime.lock();
                            runtime.last_error =
                                Some(format!("automatic retention failed: {error:#}"));
                        }
                        last_purge = Instant::now();
                    }
                    drop(manager);
                    thread::sleep(Duration::from_millis(250));
                }
            })?;
        Ok(())
    }

    fn check_automatic_cutoff(&self) -> Result<()> {
        let now = qpc_now_100ns()?;
        let due = {
            let runtime = self.runtime.lock();
            runtime
                .active
                .as_ref()
                .filter(|active| cutoff_reached(active.origin_100ns, now, active.cutoff_seconds))
                .map(|active| (active.writer.clone(), active.origin_100ns))
        };
        let Some((writer, origin)) = due else {
            return Ok(());
        };
        writer.add_event(
            now.saturating_sub(origin),
            "recorder",
            "automatic_cutoff",
            "Automatic duration cutoff reached",
            None,
            None,
            &json!({}),
            None,
        )?;
        let _ = self.finalize_active(true)?;
        Ok(())
    }

    pub fn status(&self) -> RecorderStatus {
        let runtime = self.runtime.lock();
        let (session, turn, started, elapsed) = runtime
            .active
            .as_ref()
            .map(|active| {
                (
                    Some(active.id.clone()),
                    Some(active.latest_turn_id.clone()),
                    Some(active.started_at_utc.clone()),
                    qpc_now_100ns()
                        .ok()
                        .map(|now| now.saturating_sub(active.origin_100ns) / 10_000),
                )
            })
            .unwrap_or((None, None, None, None));
        RecorderStatus {
            state: runtime.state,
            armed: runtime.armed,
            monitor_index: runtime.monitor_index,
            active_session_id: session,
            active_turn_id: turn,
            started_at_utc: started,
            elapsed_ms: elapsed,
            last_error: runtime.last_error.clone(),
            monitors: list_monitors().unwrap_or_default(),
        }
    }

    pub fn privacy_consent_status(&self) -> Result<PrivacyConsentStatus> {
        let consent = self
            .store
            .get_setting("privacy_consent_v1")?
            .and_then(|value| serde_json::from_str::<PrivacyConsent>(&value).ok());
        Ok(PrivacyConsentStatus {
            required_version: PRIVACY_CONSENT_VERSION,
            accepted: consent
                .as_ref()
                .is_some_and(|value| value.version == PRIVACY_CONSENT_VERSION),
            accepted_at_utc: consent.as_ref().map(|value| value.accepted_at_utc.clone()),
            accepted_app_version: consent.map(|value| value.app_version),
        })
    }

    fn accept_privacy_consent(&self) -> Result<PrivacyConsentStatus> {
        let consent = PrivacyConsent {
            version: PRIVACY_CONSENT_VERSION,
            accepted_at_utc: Utc::now().to_rfc3339(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        self.store
            .set_setting("privacy_consent_v1", &serde_json::to_string(&consent)?)?;
        self.privacy_consent_status()
    }

    fn require_privacy_consent(&self) -> Result<()> {
        if !self.privacy_consent_status()?.accepted {
            bail!("review and accept the Flight Recorder privacy notice before arming")
        }
        Ok(())
    }

    pub fn runtime_diagnostics(&self) -> RuntimeDiagnostics {
        let ffmpeg_path = ffmpeg_path();
        let ffprobe_path = ffprobe_path();
        let ffmpeg_output = ffmpeg_command().arg("-version").output().ok();
        let ffprobe_available = ffprobe_command()
            .arg("-version")
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        let ffmpeg_available = ffmpeg_output
            .as_ref()
            .is_some_and(|output| output.status.success());
        let ffmpeg_version = ffmpeg_output.and_then(|output| {
            String::from_utf8(output.stdout)
                .ok()
                .and_then(|value| value.lines().next().map(ToOwned::to_owned))
        });
        RuntimeDiagnostics {
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            executable_path: std::env::current_exe().unwrap_or_default(),
            control_root: self.store.root().to_path_buf(),
            log_root: self.store.root().join("logs"),
            ffmpeg_path,
            ffmpeg_available,
            ffmpeg_version,
            ffprobe_path,
            ffprobe_available,
        }
    }

    pub fn handle_request(&self, request: BridgeRequest) -> BridgeResponse {
        match self.try_handle(request) {
            Ok(response) => BridgeResponse::success(response),
            Err(error) => BridgeResponse::failure(format!("{error:#}")),
        }
    }

    fn try_handle(&self, request: BridgeRequest) -> Result<serde_json::Value> {
        match request {
            BridgeRequest::OpenRecorder => {
                let reveal_epoch = request_window_reveal();
                Ok(json!({ "opened": true, "reveal_epoch": reveal_epoch }))
            }
            BridgeRequest::GetStatus => Ok(serde_json::to_value(self.status())?),
            BridgeRequest::GetPreferences => Ok(serde_json::to_value(self.store.preferences()?)?),
            BridgeRequest::GetPrivacyConsent => {
                Ok(serde_json::to_value(self.privacy_consent_status()?)?)
            }
            BridgeRequest::AcceptPrivacyConsent => {
                Ok(serde_json::to_value(self.accept_privacy_consent()?)?)
            }
            BridgeRequest::GetRuntimeDiagnostics => {
                Ok(serde_json::to_value(self.runtime_diagnostics())?)
            }
            BridgeRequest::SetPreferences { preferences } => {
                let preferences = self.store.save_preferences(preferences)?;
                let purged = self.store.purge_expired_preferences()?;
                Ok(json!({ "preferences": preferences, "purged": purged }))
            }
            BridgeRequest::GetMcpStatus => Ok(serde_json::to_value(self.mcp_status())?),
            BridgeRequest::McpHeartbeat { heartbeat } => {
                self.record_mcp_heartbeat(heartbeat);
                Ok(json!({ "connected": true }))
            }
            BridgeRequest::McpDisconnected { instance_id } => {
                self.disconnect_mcp(&instance_id);
                Ok(json!({ "connected": false }))
            }
            BridgeRequest::Arm { monitor_index } => {
                self.require_privacy_consent()?;
                let monitor = list_monitors()?
                    .into_iter()
                    .find(|item| item.index == monitor_index)
                    .with_context(|| format!("monitor {monitor_index} was not found"))?;
                let mut runtime = self.runtime.lock();
                if runtime.active.is_some() {
                    bail!("cannot change monitors while recording");
                }
                runtime.armed = true;
                runtime.monitor_index = Some(monitor.index);
                runtime.state = RecorderState::Armed;
                runtime.last_error = None;
                Ok(json!({ "armed": true, "monitor": monitor }))
            }
            BridgeRequest::Disarm => {
                self.finalize_active(false)?;
                let mut runtime = self.runtime.lock();
                runtime.armed = false;
                runtime.monitor_index = None;
                runtime.state = RecorderState::Disarmed;
                Ok(json!({ "armed": false }))
            }
            BridgeRequest::StopNow => {
                let stopped = self.finalize_active(true)?;
                Ok(json!({ "stopped": stopped }))
            }
            BridgeRequest::ListSessions { cursor, limit } => {
                let (sessions, next_cursor) = self
                    .store
                    .list_sessions(cursor.as_deref(), limit.unwrap_or(25))?;
                Ok(json!({ "sessions": sessions, "next_cursor": next_cursor }))
            }
            BridgeRequest::GetTimeline {
                session_id,
                start_ms,
                end_ms,
                cursor,
                limit,
            } => Ok(serde_json::to_value(self.store.timeline(
                &session_id,
                start_ms,
                end_ms,
                cursor.as_deref(),
                limit.unwrap_or(100),
            )?)?),
            BridgeRequest::GetFrameAt {
                session_id,
                offset_ms,
            } => Ok(serde_json::to_value(
                self.store.extract_frame(&session_id, offset_ms)?,
            )?),
            BridgeRequest::SelectFrame {
                session_id,
                offset_ms,
            } => {
                let shared_frame = self.store.share_frame(&session_id, offset_ms)?;
                self.store.set_setting("selected_session", &session_id)?;
                self.store
                    .set_setting("selected_offset_ms", &offset_ms.to_string())?;
                let count = self.store.list_shared_frames()?.len();
                Ok(json!({ "selected": true, "shared_frame": shared_frame, "count": count }))
            }
            BridgeRequest::GetSelectedFrame => {
                if let Some(shared) = self.store.latest_shared_frame()? {
                    return Ok(serde_json::to_value(shared)?);
                }
                let session_id = self
                    .store
                    .get_setting("selected_session")?
                    .context("no frame is selected in the reviewer")?;
                let offset_ms = self
                    .store
                    .get_setting("selected_offset_ms")?
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                Ok(serde_json::to_value(
                    self.store.extract_frame(&session_id, offset_ms)?,
                )?)
            }
            BridgeRequest::ListSharedFrames => {
                let shared_frames = self.store.list_shared_frames()?;
                Ok(json!({ "count": shared_frames.len(), "shared_frames": shared_frames }))
            }
            BridgeRequest::GetSharedFrame { share_id } => Ok(serde_json::to_value(
                self.store.get_shared_frame(&share_id)?,
            )?),
            BridgeRequest::RemoveSharedFrame { share_id } => {
                let removed = self.store.remove_shared_frame(&share_id)?;
                if let Some(latest) = self.store.latest_shared_frame()? {
                    self.store
                        .set_setting("selected_session", &latest.session_id)?;
                    self.store.set_setting(
                        "selected_offset_ms",
                        &latest.requested_offset_ms.to_string(),
                    )?;
                } else {
                    self.store.remove_setting("selected_session")?;
                    self.store.remove_setting("selected_offset_ms")?;
                }
                Ok(json!({ "removed": removed, "share_id": share_id }))
            }
            BridgeRequest::ClearSharedFrames => {
                let removed = self.store.clear_shared_frames()?;
                self.store.remove_setting("selected_session")?;
                self.store.remove_setting("selected_offset_ms")?;
                Ok(json!({ "removed": removed }))
            }
            BridgeRequest::DeleteSession { session_id } => {
                self.store.delete_session(&session_id)?;
                Ok(json!({ "deleted": session_id }))
            }
            BridgeRequest::DeleteSessionConfirmed {
                session_id,
                delete_pinned,
            } => {
                self.store
                    .delete_session_confirmed(&session_id, delete_pinned)?;
                Ok(json!({ "deleted": session_id }))
            }
            BridgeRequest::RenameSession {
                session_id,
                display_name,
            } => {
                self.store
                    .rename_session(&session_id, display_name.as_deref())?;
                let saved = self.store.get_session(&session_id)?.display_name;
                Ok(json!({ "session_id": session_id, "display_name": saved }))
            }
            BridgeRequest::PinSession { session_id, pinned } => {
                self.store.pin_session(&session_id, pinned)?;
                Ok(json!({ "session_id": session_id, "pinned": pinned }))
            }
            BridgeRequest::SetRetention { days } => {
                match days {
                    Some(value) => self
                        .store
                        .set_setting("retention_days", &value.to_string())?,
                    None => self.store.set_setting("retention_days", "")?,
                }
                let deleted = self.store.purge_expired(days)?;
                Ok(json!({ "days": days, "deleted_now": deleted }))
            }
            BridgeRequest::Hook { event } => self.handle_hook(event),
        }
    }

    fn handle_hook(&self, event: HookEvent) -> Result<serde_json::Value> {
        match event.hook_event_name.as_str() {
            "UserPromptSubmit" => self.prompt_submitted(event),
            "PreToolUse" => self.tool_event(event, false),
            "PostToolUse" => self.tool_event(event, true),
            "Stop" => {
                let is_latest = {
                    let runtime = self.runtime.lock();
                    runtime
                        .active
                        .as_ref()
                        .map(|active| {
                            event.turn_id.as_deref() == Some(active.latest_turn_id.as_str())
                        })
                        .unwrap_or(false)
                };
                if !is_latest {
                    return Ok(json!({ "ignored": "stale_or_duplicate_stop" }));
                }
                if let Some(turn_id) = event.turn_id.as_deref() {
                    let runtime = self.runtime.lock();
                    if let Some(active) = runtime.active.as_ref() {
                        active.writer.end_turn(
                            turn_id,
                            event.hook_qpc_100ns.saturating_sub(active.origin_100ns),
                        )?;
                    }
                }
                thread::sleep(Duration::from_millis(500));
                let stopped = self.finalize_active(true)?;
                Ok(json!({ "stopped": stopped, "tail_ms": 500 }))
            }
            other => Ok(json!({ "ignored": other })),
        }
    }

    fn prompt_submitted(&self, event: HookEvent) -> Result<serde_json::Value> {
        // Hooks are an independent entry point, so enforce consent before making
        // any recording-state transition even if the persisted setting changes
        // after the recorder was armed.
        self.require_privacy_consent()?;
        let turn_id = event
            .turn_id
            .clone()
            .unwrap_or_else(|| event.session_id.clone());
        {
            let mut runtime = self.runtime.lock();
            if let Some(active) = runtime.active.as_mut() {
                active.input.flush_pending_text()?;
                active.writer.add_turn(
                    &turn_id,
                    event.hook_qpc_100ns.saturating_sub(active.origin_100ns),
                    event.prompt_length,
                    event.prompt_sha256.as_deref(),
                )?;
                active.latest_turn_id = turn_id;
                return Ok(json!({ "session_id": active.id, "continued": true }));
            }
            if !runtime.armed {
                return Ok(json!({ "ignored": "recorder_not_armed" }));
            }
            runtime.state = RecorderState::Recording;
        }
        match self.start_session(&event, &turn_id) {
            Ok(id) => Ok(json!({ "session_id": id, "continued": false })),
            Err(error) => {
                let mut runtime = self.runtime.lock();
                runtime.state = RecorderState::Error;
                runtime.last_error = Some(format!("{error:#}"));
                Err(error)
            }
        }
    }

    fn start_session(&self, event: &HookEvent, turn_id: &str) -> Result<String> {
        let monitor_index = self
            .runtime
            .lock()
            .monitor_index
            .context("no monitor is armed")?;
        let monitor = list_monitors()?
            .into_iter()
            .find(|item| item.index == monitor_index)
            .context("armed monitor is no longer available")?;
        let preferences = self.store.preferences()?;
        let (output_width, output_height) =
            output_dimensions(monitor.width, monitor.height, preferences.resolution);
        let id = Uuid::now_v7().to_string();
        let started = Utc::now().to_rfc3339();
        let writer = self.store.create_session(
            &id,
            &started,
            event.hook_qpc_100ns,
            qpc_frequency()?,
            &monitor.name,
            monitor.width,
            monitor.height,
            output_width,
            output_height,
        )?;
        writer.add_turn(
            turn_id,
            0,
            event.prompt_length,
            event.prompt_sha256.as_deref(),
        )?;
        let input =
            InputObserver::start(writer.clone()).context("input observation could not start")?;
        let capture = match CaptureSession::start(
            monitor_index,
            writer.clone(),
            preferences.resolution,
            preferences.quality,
        ) {
            Ok(value) => value,
            Err(error) => {
                let _ = input.stop();
                let _ = self.store.mark_session_error(&id);
                return Err(error.context("screen capture could not start"));
            }
        };
        self.runtime.lock().active = Some(ActiveSession {
            id: id.clone(),
            writer,
            capture,
            input,
            latest_turn_id: turn_id.to_owned(),
            started_at_utc: started,
            origin_100ns: event.hook_qpc_100ns,
            cutoff_seconds: preferences.cutoff_seconds,
        });
        Ok(id)
    }

    fn tool_event(&self, event: HookEvent, completed: bool) -> Result<serde_json::Value> {
        let runtime = self.runtime.lock();
        let Some(active) = runtime.active.as_ref() else {
            return Ok(json!({ "ignored": "not_recording" }));
        };
        if event.tool_name.as_deref() != Some("mcp__node_repl__js") {
            return Ok(json!({ "ignored": "tool_not_matched" }));
        }
        let offset = event.hook_qpc_100ns.saturating_sub(active.origin_100ns);
        let tool_use_id = event.tool_use_id.as_deref().unwrap_or("unknown");
        if completed {
            active
                .writer
                .upsert_tool_end(tool_use_id, "mcp__node_repl__js", offset)?;
        } else {
            active
                .writer
                .upsert_tool_start(tool_use_id, "mcp__node_repl__js", offset)?;
        }
        let mut actions = 0;
        if !completed {
            if let Some(input) = event.tool_input.as_ref() {
                for action in parse_sky_actions(input) {
                    let sensitive = action.sensitive_text.as_deref().map(str::as_bytes);
                    active.writer.add_event(
                        offset,
                        "requested_action",
                        &action.action,
                        &format!("Requested {}", action.action),
                        Some(0.75),
                        Some(tool_use_id),
                        &action.public_payload,
                        sensitive,
                    )?;
                    actions += 1;
                }
            }
        }
        Ok(json!({ "recorded": true, "actions": actions }))
    }

    fn finalize_active(&self, remain_armed: bool) -> Result<bool> {
        let active = {
            let mut runtime = self.runtime.lock();
            let Some(active) = runtime.active.take() else {
                return Ok(false);
            };
            runtime.state = RecorderState::Finalizing;
            active
        };
        let elapsed = qpc_now_100ns()?.saturating_sub(active.origin_100ns);
        let session_id = active.id.clone();
        let input_result = active.input.stop();
        let capture_result = active.capture.stop();
        let result = input_result.and(capture_result).and_then(|_| {
            let (frames, events) = active.writer.counts()?;
            self.store
                .finalize_session(&session_id, elapsed / 10_000, frames, events)?;
            let _ = self.store.generate_thumbnail(&session_id, elapsed / 10_000);
            Ok(())
        });
        let mut runtime = self.runtime.lock();
        runtime.armed = runtime.armed && remain_armed;
        match result {
            Ok(()) => {
                runtime.state = if runtime.armed {
                    RecorderState::Armed
                } else {
                    RecorderState::Disarmed
                };
                runtime.last_error = None;
                Ok(true)
            }
            Err(error) => {
                let _ = self.store.mark_session_error(&session_id);
                runtime.state = RecorderState::Error;
                runtime.last_error = Some(format!("{error:#}"));
                Err(error)
            }
        }
    }
}

fn cutoff_reached(origin_100ns: i64, now_100ns: i64, cutoff_seconds: Option<u64>) -> bool {
    let Some(seconds) = cutoff_seconds else {
        return false;
    };
    let cutoff_100ns = i64::try_from(seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(10_000_000);
    now_100ns.saturating_sub(origin_100ns) >= cutoff_100ns
}

#[cfg(test)]
mod manager_tests {
    use super::{
        RecorderManager, RuntimeState, cutoff_reached, request_window_reveal, window_reveal_epoch,
    };
    use crate::model::{BridgeRequest, McpHeartbeat, RecorderState};
    use crate::store::Store;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    #[test]
    fn open_recorder_requests_are_observable_by_the_window_controller() {
        let before = window_reveal_epoch();
        let requested = request_window_reveal();
        let observed = window_reveal_epoch();

        assert_ne!(requested, before);
        assert_eq!(observed, requested);
    }

    #[test]
    fn cutoff_uses_monotonic_session_offsets() {
        let origin = 5_000_000;
        assert!(!cutoff_reached(origin, origin + 19_999_999, Some(2)));
        assert!(cutoff_reached(origin, origin + 20_000_000, Some(2)));
        assert!(!cutoff_reached(origin, i64::MAX, None));
    }

    #[test]
    fn real_mcp_heartbeat_reports_live_process_details_and_disconnects() {
        let root =
            std::env::temp_dir().join(format!("cdxvidext-mcp-presence-{}", uuid::Uuid::now_v7()));
        let manager = RecorderManager {
            store: Store::open(root.clone()).unwrap(),
            runtime: Mutex::new(RuntimeState {
                state: RecorderState::Disarmed,
                armed: false,
                monitor_index: None,
                active: None,
                last_error: None,
            }),
            mcp_instances: Mutex::new(HashMap::new()),
        };
        let heartbeat = McpHeartbeat {
            instance_id: uuid::Uuid::now_v7().to_string(),
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").into(),
            executable_path: std::env::current_exe().unwrap(),
            started_at_utc: chrono::Utc::now().to_rfc3339(),
        };

        manager.record_mcp_heartbeat(heartbeat.clone());
        let connected = manager.mcp_status();
        assert!(connected.connected);
        assert_eq!(connected.active_instances, 1);
        assert_eq!(connected.primary.unwrap().pid, std::process::id());

        manager.disconnect_mcp(&heartbeat.instance_id);
        assert!(!manager.mcp_status().connected);
        drop(manager);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_store_requires_and_persists_versioned_privacy_consent() {
        let root = std::env::temp_dir().join(format!("cdxvidext-consent-{}", uuid::Uuid::now_v7()));
        let manager = RecorderManager {
            store: Store::open(root.clone()).unwrap(),
            runtime: Mutex::new(RuntimeState {
                state: RecorderState::Disarmed,
                armed: false,
                monitor_index: None,
                active: None,
                last_error: None,
            }),
            mcp_instances: Mutex::new(HashMap::new()),
        };

        let rejected = manager.handle_request(BridgeRequest::Arm { monitor_index: 0 });
        assert!(!rejected.ok);
        assert!(rejected.error.unwrap().contains("privacy notice"));

        let accepted = manager.handle_request(BridgeRequest::AcceptPrivacyConsent);
        assert!(accepted.ok);
        assert_eq!(accepted.data["accepted"], true);
        drop(manager);

        let reopened = Store::open(root.clone()).unwrap();
        let raw = reopened
            .get_setting("privacy_consent_v1")
            .unwrap()
            .expect("consent setting");
        assert!(raw.contains("accepted_at_utc"));
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
}
