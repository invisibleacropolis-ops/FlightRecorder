use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const PROTOCOL_VERSION: u32 = 1;
pub const PRIVACY_CONSENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrivacyConsent {
    pub version: u32,
    pub accepted_at_utc: String,
    pub app_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrivacyConsentStatus {
    pub required_version: u32,
    pub accepted: bool,
    pub accepted_at_utc: Option<String>,
    pub accepted_app_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeDiagnostics {
    pub app_version: String,
    pub executable_path: PathBuf,
    pub control_root: PathBuf,
    pub log_root: PathBuf,
    pub ffmpeg_path: PathBuf,
    pub ffmpeg_available: bool,
    pub ffmpeg_version: Option<String>,
    pub ffprobe_path: PathBuf,
    pub ffprobe_available: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureQuality {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CaptureResolution {
    Hd1080,
    Qhd2k,
    Native,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub enabled: bool,
    pub days: u32,
    pub applies_after_utc: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Preferences {
    pub flight_root: PathBuf,
    pub snapshot_root: PathBuf,
    pub flight_retention: RetentionPolicy,
    pub snapshot_retention: RetentionPolicy,
    pub cutoff_seconds: Option<u64>,
    pub quality: CaptureQuality,
    pub resolution: CaptureResolution,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PurgeResult {
    pub flights_deleted: usize,
    pub snapshots_deleted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpHeartbeat {
    pub instance_id: String,
    pub pid: u32,
    pub version: String,
    pub executable_path: PathBuf,
    pub started_at_utc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpInstanceStatus {
    pub instance_id: String,
    pub pid: u32,
    pub version: String,
    pub executable_path: PathBuf,
    pub started_at_utc: String,
    pub last_seen_utc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpConnectionStatus {
    pub connected: bool,
    pub active_instances: usize,
    pub primary: Option<McpInstanceStatus>,
}

impl Preferences {
    pub fn defaults_for(control_root: &Path) -> Self {
        Self {
            flight_root: control_root.join("sessions"),
            snapshot_root: control_root.join("exports"),
            flight_retention: RetentionPolicy {
                enabled: false,
                days: 30,
                applies_after_utc: None,
            },
            snapshot_retention: RetentionPolicy {
                enabled: false,
                days: 30,
                applies_after_utc: None,
            },
            cutoff_seconds: None,
            quality: CaptureQuality::Medium,
            resolution: CaptureResolution::Hd1080,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecorderState {
    Disarmed,
    Armed,
    Recording,
    Finalizing,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderStatus {
    pub state: RecorderState,
    pub armed: bool,
    pub monitor_index: Option<usize>,
    pub active_session_id: Option<String>,
    pub active_turn_id: Option<String>,
    pub started_at_utc: Option<String>,
    pub elapsed_ms: Option<i64>,
    pub last_error: Option<String>,
    pub monitors: Vec<MonitorInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub started_at_utc: String,
    pub ended_at_utc: Option<String>,
    pub state: String,
    pub duration_ms: Option<i64>,
    pub monitor_name: String,
    pub output_width: u32,
    pub output_height: u32,
    pub frame_count: i64,
    pub event_count: i64,
    pub pinned: bool,
    pub media_path: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub event_id: i64,
    pub offset_100ns: i64,
    pub source: String,
    pub kind: String,
    pub summary: String,
    pub confidence: Option<f64>,
    pub tool_use_id: Option<String>,
    pub public_payload: Value,
    pub has_encrypted_payload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelinePage {
    pub session_id: String,
    pub events: Vec<TimelineEvent>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameEvidence {
    pub session_id: String,
    pub frame_number: i64,
    pub offset_100ns: i64,
    pub offset_ms: f64,
    pub image_path: String,
    pub mime_type: String,
    pub nearest_event: Option<TimelineEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedFrame {
    pub share_id: String,
    pub session_id: String,
    pub requested_offset_ms: i64,
    pub frame_number: i64,
    pub offset_100ns: i64,
    pub offset_ms: f64,
    pub image_path: String,
    pub mime_type: String,
    pub created_at_utc: String,
    pub nearest_event: Option<TimelineEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvent {
    pub hook_event_name: String,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_response: Option<Value>,
    pub prompt_length: Option<usize>,
    pub prompt_sha256: Option<String>,
    pub hook_qpc_100ns: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeRequest {
    OpenRecorder,
    GetStatus,
    GetPreferences,
    GetPrivacyConsent,
    AcceptPrivacyConsent,
    GetRuntimeDiagnostics,
    SetPreferences {
        preferences: Preferences,
    },
    GetMcpStatus,
    McpHeartbeat {
        heartbeat: McpHeartbeat,
    },
    McpDisconnected {
        instance_id: String,
    },
    Arm {
        monitor_index: usize,
    },
    Disarm,
    StopNow,
    ListSessions {
        cursor: Option<String>,
        limit: Option<usize>,
    },
    GetTimeline {
        session_id: String,
        start_ms: Option<i64>,
        end_ms: Option<i64>,
        cursor: Option<String>,
        limit: Option<usize>,
    },
    GetFrameAt {
        session_id: String,
        offset_ms: i64,
    },
    GetSelectedFrame,
    ListSharedFrames,
    GetSharedFrame {
        share_id: String,
    },
    RemoveSharedFrame {
        share_id: String,
    },
    ClearSharedFrames,
    SelectFrame {
        session_id: String,
        offset_ms: i64,
    },
    DeleteSession {
        session_id: String,
    },
    DeleteSessionConfirmed {
        session_id: String,
        delete_pinned: bool,
    },
    RenameSession {
        session_id: String,
        display_name: Option<String>,
    },
    PinSession {
        session_id: String,
        pinned: bool,
    },
    SetRetention {
        days: Option<u32>,
    },
    Hook {
        event: HookEvent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeResponse {
    pub protocol_version: u32,
    pub ok: bool,
    pub data: Value,
    pub error: Option<String>,
}

impl BridgeResponse {
    pub fn success<T: Serialize>(value: T) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            data: serde_json::to_value(value).unwrap_or(Value::Null),
            error: None,
        }
    }

    pub fn failure(message: impl Into<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: false,
            data: Value::Null,
            error: Some(message.into()),
        }
    }
}
