use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use cdxvidext_core::clock::qpc_now_100ns;
use cdxvidext_core::ipc::send_request;
use cdxvidext_core::model::{BridgeRequest, BridgeResponse, HookEvent, McpHeartbeat, Preferences};
use cdxvidext_core::store::Store;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
struct RecorderMcp;

#[derive(Debug, Deserialize, JsonSchema)]
struct PageInput {
    #[schemars(description = "Opaque pagination cursor returned by the previous call")]
    cursor: Option<String>,
    #[schemars(description = "Maximum records to return (1-100)")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TimelineInput {
    session_id: String,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FrameInput {
    session_id: String,
    offset_ms: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SharedFrameInput {
    #[schemars(description = "Stable share ID returned by list_shared_frames")]
    share_id: String,
}

impl RecorderMcp {
    fn new() -> Self {
        Self
    }
}

#[tool_router]
impl RecorderMcp {
    #[tool(
        description = "Open the persistent Flight Recorder and return its live state. Use this before asking the user to arm a monitor."
    )]
    fn open_recorder(&self) -> CallToolResult {
        match ensure_recorder() {
            Ok(()) => structured(send_request(&BridgeRequest::OpenRecorder)),
            Err(error) => tool_error(error),
        }
    }

    #[tool(
        description = "Get recorder state, armed monitor, active session and turn, elapsed time, errors, and available monitors."
    )]
    fn get_recording_status(&self) -> CallToolResult {
        structured(send_request(&BridgeRequest::GetStatus))
    }

    #[tool(
        description = "List local recording sessions newest first. Results contain stable session IDs and an opaque next cursor."
    )]
    fn list_recording_sessions(&self, Parameters(input): Parameters<PageInput>) -> CallToolResult {
        structured(send_request(&BridgeRequest::ListSessions {
            cursor: input.cursor,
            limit: input.limit,
        }))
    }

    #[tool(
        description = "Read a time-bounded, paginated event timeline for one real recorded session."
    )]
    fn get_session_timeline(&self, Parameters(input): Parameters<TimelineInput>) -> CallToolResult {
        structured(send_request(&BridgeRequest::GetTimeline {
            session_id: input.session_id,
            start_ms: input.start_ms,
            end_ms: input.end_ms,
            cursor: input.cursor,
            limit: input.limit,
        }))
    }

    #[tool(
        description = "Extract and return the exact PNG frame nearest a millisecond offset, with structured timing and nearest-action evidence."
    )]
    fn get_frame_at(&self, Parameters(input): Parameters<FrameInput>) -> CallToolResult {
        image_result(send_request(&BridgeRequest::GetFrameAt {
            session_id: input.session_id,
            offset_ms: input.offset_ms,
        }))
    }

    #[tool(
        description = "Return the PNG frame currently selected in the desktop reviewer, plus session, timing, nearest action, and confidence metadata."
    )]
    fn get_selected_frame(&self) -> CallToolResult {
        image_result(send_request(&BridgeRequest::GetSelectedFrame))
    }

    #[tool(
        description = "List the persistent Shared frames tray exactly as it appears in the desktop recorder, oldest to newest, with stable share IDs and timing metadata."
    )]
    fn list_shared_frames(&self) -> CallToolResult {
        structured(send_request(&BridgeRequest::ListSharedFrames))
    }

    #[tool(
        description = "Return one exact PNG from the desktop Shared frames tray by its stable share ID."
    )]
    fn get_shared_frame(&self, Parameters(input): Parameters<SharedFrameInput>) -> CallToolResult {
        image_result(send_request(&BridgeRequest::GetSharedFrame {
            share_id: input.share_id,
        }))
    }

    #[tool(
        description = "Return every exact PNG currently visible in the desktop Shared frames tray, in oldest-to-newest order, with structured metadata for the complete stack."
    )]
    fn get_shared_frames(&self) -> CallToolResult {
        shared_images_result(send_request(&BridgeRequest::ListSharedFrames))
    }
}

#[tool_handler]
impl ServerHandler for RecorderMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("cdxvidext-flight-recorder", env!("CARGO_PKG_VERSION")))
            .with_instructions("Use session IDs and opaque cursors exactly as returned. Frame tools return real local PNG evidence.")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "open".into());
    match mode.as_str() {
        "hook" => run_hook(),
        "mcp" => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter("warn")
                .init();
            let heartbeat = McpHeartbeat {
                instance_id: uuid::Uuid::now_v7().to_string(),
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").into(),
                executable_path: std::env::current_exe()?,
                started_at_utc: chrono::Utc::now().to_rfc3339(),
            };
            let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::sync_channel::<()>(1);
            let heartbeat_worker = heartbeat.clone();
            std::thread::spawn(move || {
                loop {
                    let _ = send_request(&BridgeRequest::McpHeartbeat {
                        heartbeat: heartbeat_worker.clone(),
                    });
                    if shutdown_receiver
                        .recv_timeout(Duration::from_secs(2))
                        .is_ok()
                    {
                        return;
                    }
                }
            });
            let result = RecorderMcp::new()
                .serve(rmcp::transport::stdio())
                .await?
                .waiting()
                .await;
            let _ = shutdown_sender.send(());
            let _ = send_request(&BridgeRequest::McpDisconnected {
                instance_id: heartbeat.instance_id,
            });
            result?;
            Ok(())
        }
        "open" => {
            ensure_recorder()?;
            print_pipe_response(BridgeRequest::OpenRecorder)
        }
        "verify" => {
            let session_id = std::env::args()
                .nth(2)
                .context("verify requires a session id")?;
            let report = Store::open_default()?.verification_report(&session_id)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(())
        }
        "status" => print_pipe_response(BridgeRequest::GetStatus),
        "preferences" => print_pipe_response(BridgeRequest::GetPreferences),
        "privacy-consent" => print_pipe_response(BridgeRequest::GetPrivacyConsent),
        "accept-privacy-consent" => print_pipe_response(BridgeRequest::AcceptPrivacyConsent),
        "diagnostics" => print_pipe_response(BridgeRequest::GetRuntimeDiagnostics),
        "set-preferences" => {
            let path = std::env::args()
                .nth(2)
                .context("set-preferences requires a JSON file path")?;
            let preferences: Preferences = serde_json::from_slice(&std::fs::read(path)?)?;
            print_pipe_response(BridgeRequest::SetPreferences { preferences })
        }
        "mcp-status" => print_pipe_response(BridgeRequest::GetMcpStatus),
        "frame" => {
            let session_id = std::env::args()
                .nth(2)
                .context("frame requires a session id")?;
            let offset_ms = std::env::args()
                .nth(3)
                .context("frame requires an offset in milliseconds")?
                .parse::<i64>()?;
            print_pipe_response(BridgeRequest::GetFrameAt {
                session_id,
                offset_ms,
            })
        }
        "arm" => {
            let monitor_index = std::env::args()
                .nth(2)
                .context("arm requires a one-based monitor index")?
                .parse::<usize>()?;
            print_pipe_response(BridgeRequest::Arm { monitor_index })
        }
        "stop" => print_pipe_response(BridgeRequest::StopNow),
        other => bail!("unknown bridge mode: {other}"),
    }
}

fn print_pipe_response(request: BridgeRequest) -> Result<()> {
    let response = send_request(&request)?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    if !response.ok {
        bail!(
            response
                .error
                .unwrap_or_else(|| "recorder request failed".into())
        );
    }
    Ok(())
}

fn run_hook() -> Result<()> {
    // Parse one complete value instead of reading to EOF. Codex may keep the
    // hook's stdin pipe open while it waits for the process to exit, so an
    // EOF-dependent read can deadlock until the outer hook timeout.
    let payload = serde_json::Deserializer::from_reader(std::io::stdin().lock())
        .into_iter::<Value>()
        .next()
        .transpose()
        .context("hook input is not valid JSON")?
        .context("hook input is empty")?;
    let prompt = payload.get("prompt").and_then(Value::as_str);
    let event = HookEvent {
        hook_event_name: string_field(&payload, &["hook_event_name", "hookEventName"])
            .unwrap_or_else(|| "Unknown".into()),
        session_id: string_field(&payload, &["session_id", "sessionId", "conversation_id"])
            .unwrap_or_else(|| "unknown-session".into()),
        turn_id: string_field(&payload, &["turn_id", "turnId"]),
        tool_name: string_field(&payload, &["tool_name", "toolName"]),
        tool_use_id: string_field(&payload, &["tool_use_id", "toolUseId", "tool_call_id"]),
        tool_input: payload
            .get("tool_input")
            .or_else(|| payload.get("toolInput"))
            .cloned(),
        // Tool responses can contain multi-megabyte screenshots. The recorder
        // never persists or interprets them, so do not copy them into IPC.
        tool_response: None,
        prompt_length: prompt.map(str::chars).map(Iterator::count),
        prompt_sha256: prompt.map(|value| format!("{:x}", Sha256::digest(value.as_bytes()))),
        hook_qpc_100ns: qpc_now_100ns()?,
    };
    let acknowledgement_timeout = if event.hook_event_name == "Stop" {
        Duration::from_millis(5_500)
    } else {
        Duration::from_millis(500)
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(send_request(&BridgeRequest::Hook { event }));
    });
    let output = match receiver.recv_timeout(acknowledgement_timeout) {
        Ok(Ok(response)) if response.ok => json!({}),
        Ok(Ok(response)) => {
            json!({ "systemMessage": format!("Flight Recorder warning: {}", response.error.unwrap_or_else(|| "recording request failed".into())) })
        }
        Ok(Err(error)) => json!({ "systemMessage": format!("Flight Recorder warning: {error}") }),
        Err(_) => {
            json!({ "systemMessage": format!("Flight Recorder warning: recorder acknowledgement exceeded {} ms", acknowledgement_timeout.as_millis()) })
        }
    };
    serde_json::to_writer(std::io::stdout(), &output)?;
    std::io::stdout().write_all(b"\n")?;
    Ok(())
}

fn string_field(value: &Value, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        value
            .get(*field)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn ensure_recorder() -> Result<()> {
    if send_request(&BridgeRequest::GetStatus).is_ok() {
        return Ok(());
    }
    launch_desktop()?;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(100));
        if send_request(&BridgeRequest::GetStatus).is_ok() {
            return Ok(());
        }
    }
    bail!("the recorder did not become ready within two seconds")
}

fn launch_desktop() -> Result<()> {
    let current = std::env::current_exe()?;
    let desktop = current
        .parent()
        .context("bridge has no parent directory")?
        .join("cdxvidext-desktop.exe");
    if !desktop.exists() {
        bail!("desktop recorder is missing at {}", desktop.display())
    }
    Command::new(desktop)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .context("failed to launch desktop recorder")?;
    Ok(())
}

fn structured(response: Result<BridgeResponse>) -> CallToolResult {
    match response {
        Ok(response) if response.ok => CallToolResult::structured(response.data),
        Ok(response) => CallToolResult::structured_error(json!({ "error": response.error })),
        Err(error) => tool_error(error),
    }
}

fn image_result(response: Result<BridgeResponse>) -> CallToolResult {
    let response = match response {
        Ok(response) if response.ok => response,
        Ok(response) => {
            return CallToolResult::structured_error(json!({ "error": response.error }));
        }
        Err(error) => return tool_error(error),
    };
    let Some(path) = response.data.get("image_path").and_then(Value::as_str) else {
        return CallToolResult::structured_error(
            json!({ "error": "recorder returned no image path" }),
        );
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => return tool_error(error),
    };
    let mut result = CallToolResult::structured(response.data.clone());
    result.content = vec![
        ContentBlock::text(response.data.to_string()),
        ContentBlock::image(STANDARD.encode(bytes), "image/png"),
    ];
    result
}

fn shared_images_result(response: Result<BridgeResponse>) -> CallToolResult {
    let response = match response {
        Ok(response) if response.ok => response,
        Ok(response) => {
            return CallToolResult::structured_error(json!({ "error": response.error }));
        }
        Err(error) => return tool_error(error),
    };
    let Some(frames) = response.data.get("shared_frames").and_then(Value::as_array) else {
        return CallToolResult::structured_error(
            json!({ "error": "recorder returned no shared frame collection" }),
        );
    };
    let mut content = vec![ContentBlock::text(response.data.to_string())];
    for frame in frames {
        let Some(path) = frame.get("image_path").and_then(Value::as_str) else {
            continue;
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => return tool_error(error),
        };
        content.push(ContentBlock::image(STANDARD.encode(bytes), "image/png"));
    }
    let mut result = CallToolResult::structured(response.data);
    result.content = content;
    result
}

fn tool_error(error: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(error.to_string())])
}

#[cfg(windows)]
trait WindowsProcessFlags {
    fn creation_flags(&mut self, flags: u32) -> &mut Self;
}
#[cfg(windows)]
impl WindowsProcessFlags for Command {
    fn creation_flags(&mut self, flags: u32) -> &mut Self {
        use std::os::windows::process::CommandExt;
        CommandExt::creation_flags(self, flags)
    }
}
