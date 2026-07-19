#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use cdxvidext_core::RecorderManager;
use cdxvidext_core::ipc;
use cdxvidext_core::manager::window_reveal_epoch;
use cdxvidext_core::model::BridgeRequest;
use cdxvidext_core::{McpConnectionStatus, Preferences};
use chrono::{DateTime, Datelike, Local, Timelike};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{Value, json};
use tauri::{
    AppHandle, LogicalSize, Manager, PhysicalPosition, PhysicalRect, PhysicalSize, WebviewUrl,
    WebviewWindow, WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tower::ServiceExt;
use tower_http::services::ServeFile;

#[derive(Clone)]
struct AppState {
    manager: Arc<RecorderManager>,
    auth_token: Arc<str>,
    allowed_origin: Arc<str>,
}

#[derive(Deserialize)]
struct BootstrapQuery {
    token: String,
}

#[derive(Deserialize)]
struct TimelineQuery {
    start_ms: Option<i64>,
    end_ms: Option<i64>,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct Selection {
    session_id: String,
    offset_ms: i64,
}

#[derive(Deserialize)]
struct Retention {
    days: Option<u32>,
}

#[derive(Deserialize)]
struct RenameRequest {
    display_name: Option<String>,
}

#[derive(Deserialize)]
struct DeleteRequest {
    delete_pinned: bool,
}

#[derive(Deserialize)]
struct FolderRequest {
    current: String,
}

const COMPACT_WINDOW_WIDTH: u32 = 620;
// Windows adds a thin DWM resize frame around the borderless webview. A
// 73-pixel client height produces an observed 82-pixel toolbar, matching the
// reference footprint on the target machine.
const COMPACT_WINDOW_HEIGHT: u32 = 73;
const COMPACT_WINDOW_MARGIN: i32 = 12;
const NORMAL_WINDOW_WIDTH: f64 = 1380.0;
const NORMAL_WINDOW_HEIGHT: f64 = 880.0;
const NORMAL_MIN_WIDTH: f64 = 1080.0;
const NORMAL_MIN_HEIGHT: f64 = 700.0;

#[derive(Clone, Copy)]
struct NormalWindowGeometry {
    position: PhysicalPosition<i32>,
    inner_size: PhysicalSize<u32>,
    maximized: bool,
}

fn main() -> Result<()> {
    let log_root = cdxvidext_core::store::data_root()?.join("logs");
    std::fs::create_dir_all(&log_root)?;
    let file_appender = tracing_appender::rolling::daily(log_root, "flight-recorder.log");
    let (log_writer, _log_guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_writer(log_writer)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cdxvidext=info".into()),
        )
        .init();

    let manager = RecorderManager::open_default()?;
    let pipe_manager = manager.clone();
    std::thread::Builder::new()
        .name("cdx-pipe-server".into())
        .spawn(move || {
            if let Err(error) = ipc::serve(pipe_manager) {
                tracing::error!(%error, "pipe server stopped");
            }
        })?;
    let mut token_bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut token_bytes);
    let auth_token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);
    let address = start_web_server(manager.clone(), auth_token.clone())?;
    let review_url = format!("http://{address}/bootstrap?token={auth_token}").parse()?;

    let hotkey_manager = manager.clone();
    let window_mode_manager = manager.clone();
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |_app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        let _ = hotkey_manager.handle_request(BridgeRequest::StopNow);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            app.global_shortcut().register("Ctrl+Alt+Shift+F12")?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(review_url))
                .title("Flight Recorder")
                .inner_size(1380.0, 880.0)
                .min_inner_size(1080.0, 700.0)
                .build()?;
            start_window_mode_controller(app.handle().clone(), window_mode_manager.clone())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .context("Tauri runtime failed")?;
    Ok(())
}

fn start_window_mode_controller(app: AppHandle, manager: Arc<RecorderManager>) -> Result<()> {
    std::thread::Builder::new()
        .name("cdx-window-mode".into())
        .spawn(move || {
            let mut compact = false;
            let mut normal_geometry = None;
            let mut handled_reveal_epoch = 0;
            loop {
                let recording = matches!(
                    manager.status().state,
                    cdxvidext_core::RecorderState::Recording
                );
                let reveal_epoch = window_reveal_epoch();
                let reveal_requested = reveal_epoch != handled_reveal_epoch;
                if recording != compact || reveal_requested {
                    let Some(window) = app.get_webview_window("main") else {
                        return;
                    };
                    if recording != compact && recording {
                        match enter_compact_mode(&window) {
                            Ok(geometry) => {
                                normal_geometry = Some(geometry);
                                compact = true;
                            }
                            Err(error) => {
                                tracing::error!(%error, "could not enter compact recording mode")
                            }
                        }
                    } else if recording != compact {
                        if let Err(error) = leave_compact_mode(&window, normal_geometry) {
                            tracing::error!(%error, "could not restore the reviewer window");
                        } else {
                            compact = false;
                        }
                    }
                    if reveal_requested {
                        let result = if recording {
                            reveal_compact_window(&window)
                        } else {
                            reveal_normal_window(&window, normal_geometry)
                        };
                        match result {
                            Ok(()) => handled_reveal_epoch = reveal_epoch,
                            Err(error) => {
                                tracing::error!(%error, reveal_epoch, "could not reveal the recorder window")
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        })?;
    Ok(())
}

fn enter_compact_mode(window: &WebviewWindow) -> Result<NormalWindowGeometry> {
    let (minimum_size, fallback_size) = normal_window_sizes(window)?;
    let geometry = NormalWindowGeometry {
        position: window.outer_position()?,
        inner_size: normalized_normal_size(window.inner_size()?, minimum_size, fallback_size),
        maximized: window.is_maximized()?,
    };
    let monitor = window
        .current_monitor()?
        .context("the recorder window is not on an active monitor")?;
    if geometry.maximized {
        window.unmaximize()?;
    }
    window.set_min_size(None::<PhysicalSize<u32>>)?;
    window.set_resizable(false)?;
    window.set_decorations(false)?;
    window.set_always_on_top(true)?;
    window.set_size(PhysicalSize::new(
        COMPACT_WINDOW_WIDTH,
        COMPACT_WINDOW_HEIGHT,
    ))?;
    let actual_size = window.outer_size()?;
    window.set_position(compact_window_position(
        *monitor.work_area(),
        actual_size,
        COMPACT_WINDOW_MARGIN,
    ))?;
    window.show()?;
    Ok(geometry)
}

fn leave_compact_mode(
    window: &WebviewWindow,
    normal_geometry: Option<NormalWindowGeometry>,
) -> Result<()> {
    window.set_always_on_top(false)?;
    window.set_decorations(true)?;
    window.set_resizable(true)?;
    window.set_min_size(Some(LogicalSize::new(NORMAL_MIN_WIDTH, NORMAL_MIN_HEIGHT)))?;
    window.unminimize()?;
    if window.is_maximized()? {
        window.unmaximize()?;
    }
    if let Some(geometry) = normal_geometry {
        let (minimum_size, fallback_size) = normal_window_sizes(window)?;
        window.set_size(normalized_normal_size(
            geometry.inner_size,
            minimum_size,
            fallback_size,
        ))?;
        window.set_position(geometry.position)?;
        if geometry.maximized {
            window.maximize()?;
        }
    } else {
        window.set_size(LogicalSize::new(NORMAL_WINDOW_WIDTH, NORMAL_WINDOW_HEIGHT))?;
        window.center()?;
    }
    window.show()?;
    window.set_focus()?;
    Ok(())
}

fn reveal_compact_window(window: &WebviewWindow) -> Result<()> {
    window.unminimize()?;
    window.show()?;
    Ok(())
}

fn reveal_normal_window(
    window: &WebviewWindow,
    normal_geometry: Option<NormalWindowGeometry>,
) -> Result<()> {
    let (minimum_size, _) = normal_window_sizes(window)?;
    let current_size = window.inner_size()?;
    if window.is_minimized()?
        || !window.is_visible()?
        || current_size.width < minimum_size.width
        || current_size.height < minimum_size.height
    {
        return leave_compact_mode(window, normal_geometry);
    }
    window.show()?;
    window.set_focus()?;
    Ok(())
}

fn normal_window_sizes(window: &WebviewWindow) -> Result<(PhysicalSize<u32>, PhysicalSize<u32>)> {
    let scale_factor = window.scale_factor()?;
    Ok((
        LogicalSize::new(NORMAL_MIN_WIDTH, NORMAL_MIN_HEIGHT).to_physical(scale_factor),
        LogicalSize::new(NORMAL_WINDOW_WIDTH, NORMAL_WINDOW_HEIGHT).to_physical(scale_factor),
    ))
}

fn normalized_normal_size(
    candidate: PhysicalSize<u32>,
    minimum: PhysicalSize<u32>,
    fallback: PhysicalSize<u32>,
) -> PhysicalSize<u32> {
    if candidate.width < minimum.width || candidate.height < minimum.height {
        fallback
    } else {
        candidate
    }
}

fn compact_window_position(
    work_area: PhysicalRect<i32, u32>,
    window_size: PhysicalSize<u32>,
    margin: i32,
) -> PhysicalPosition<i32> {
    PhysicalPosition::new(
        work_area.position.x + work_area.size.width as i32 - window_size.width as i32 - margin,
        work_area.position.y + work_area.size.height as i32 - window_size.height as i32 - margin,
    )
}

fn start_web_server(manager: Arc<RecorderManager>, auth_token: String) -> Result<SocketAddr> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("cdx-review-server".into())
        .spawn(move || {
            let runtime = tokio::runtime::Runtime::new().expect("review runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind review server");
                let address = listener.local_addr().expect("review address");
                sender.send(address).expect("publish review address");
                let state = AppState {
                    manager,
                    auth_token: auth_token.into(),
                    allowed_origin: format!("http://{address}").into(),
                };
                axum::serve(listener, routes(state))
                    .await
                    .expect("review server");
            });
        })?;
    Ok(receiver.recv()?)
}

fn routes(state: AppState) -> Router {
    let protected = Router::new()
        .route("/", get(index))
        .route("/app.css", get(css))
        .route("/app.js", get(js))
        .route("/htmx.min.js", get(htmx))
        .route("/api/status", get(status))
        .route("/api/consent", get(consent).post(accept_consent))
        .route("/api/preferences", get(preferences).post(save_preferences))
        .route("/api/preferences/browse", post(browse_folder))
        .route("/api/mcp-status", get(mcp_status))
        .route("/api/sessions", get(sessions))
        .route("/api/timeline/{session_id}", get(timeline))
        .route("/api/decrypt/{session_id}/{event_id}", get(decrypt_event))
        .route("/api/media/{session_id}", get(media))
        .route("/api/shared", get(shared_frames))
        .route("/api/shared/{share_id}/image", get(shared_image))
        .route("/api/shared/{share_id}/remove", post(remove_shared_frame))
        .route("/api/shared/clear", post(clear_shared_frames))
        .route("/api/arm/{monitor_index}", post(arm))
        .route("/api/disarm", post(disarm))
        .route("/api/stop", post(stop))
        .route("/api/select", post(select_frame))
        .route("/api/pin/{session_id}/{pinned}", post(pin))
        .route("/api/delete/{session_id}", post(delete_session))
        .route("/api/rename/{session_id}", post(rename_session))
        .route("/api/timeline-map/{session_id}", get(timeline_map))
        .route("/api/retention", post(retention))
        .route("/partials/status", get(status_partial))
        .route("/partials/preferences", get(preferences_partial))
        .route("/partials/mcp-status", get(mcp_status_partial))
        .route("/partials/sessions", get(sessions_partial))
        .route("/partials/timeline/{session_id}", get(timeline_partial))
        .route("/partials/telemetry/{session_id}", get(telemetry_partial))
        .route(
            "/partials/event/{session_id}/{event_key}",
            get(event_detail_partial),
        )
        .route("/partials/shared", get(shared_partial))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_reviewer_auth,
        ));
    Router::new()
        .route("/bootstrap", get(bootstrap))
        .merge(protected)
        .with_state(state)
}

async fn bootstrap(State(state): State<AppState>, Query(query): Query<BootstrapQuery>) -> Response {
    if !constant_time_equal(query.token.as_bytes(), state.auth_token.as_bytes()) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut response = Redirect::to("/").into_response();
    let cookie = format!(
        "cdxvidext_session={}; HttpOnly; SameSite=Strict; Path=/",
        state.auth_token
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("generated cookie is valid"),
    );
    apply_security_headers(&mut response);
    response
}

async fn require_reviewer_auth(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let authenticated = request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                (name == "cdxvidext_session").then_some(value)
            })
        })
        .is_some_and(|value| constant_time_equal(value.as_bytes(), state.auth_token.as_bytes()));
    if !authenticated {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    if request.method() != Method::GET && request.method() != Method::HEAD {
        let valid_origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|origin| origin == state.allowed_origin.as_ref());
        if !valid_origin {
            return StatusCode::FORBIDDEN.into_response();
        }
    }

    let mut response = next.run(request).await;
    apply_security_headers(&mut response);
    response
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

fn apply_security_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../ui/index.html"))
}
async fn css() -> impl IntoResponse {
    (
        [("content-type", "text/css; charset=utf-8")],
        include_str!("../ui/app.css"),
    )
}
async fn js() -> impl IntoResponse {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        include_str!("../ui/app.js"),
    )
}
async fn htmx() -> impl IntoResponse {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        include_str!("../ui/htmx.min.js"),
    )
}

async fn status(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(state.manager.status()).unwrap())
}
async fn consent(State(state): State<AppState>) -> Json<Value> {
    Json(response_data(
        state
            .manager
            .handle_request(BridgeRequest::GetPrivacyConsent),
    ))
}
async fn accept_consent(State(state): State<AppState>) -> Json<Value> {
    Json(response_data(
        state
            .manager
            .handle_request(BridgeRequest::AcceptPrivacyConsent),
    ))
}
async fn preferences(State(state): State<AppState>) -> Json<Value> {
    Json(response_data(
        state.manager.handle_request(BridgeRequest::GetPreferences),
    ))
}
async fn save_preferences(
    State(state): State<AppState>,
    Json(preferences): Json<Preferences>,
) -> Json<Value> {
    Json(response_data(state.manager.handle_request(
        BridgeRequest::SetPreferences { preferences },
    )))
}
async fn browse_folder(Json(body): Json<FolderRequest>) -> Json<Value> {
    let current = body.current;
    let selected = tokio::task::spawn_blocking(move || {
        let mut dialog = rfd::FileDialog::new();
        if !current.trim().is_empty() {
            dialog = dialog.set_directory(current);
        }
        dialog.pick_folder()
    })
    .await
    .ok()
    .flatten();
    Json(json!({ "path": selected.map(|path| path.to_string_lossy().into_owned()) }))
}
async fn mcp_status(State(state): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(state.manager.mcp_status()).unwrap_or(Value::Null))
}
async fn sessions(State(state): State<AppState>) -> Json<Value> {
    Json(response_data(state.manager.handle_request(
        BridgeRequest::ListSessions {
            cursor: None,
            limit: Some(100),
        },
    )))
}
async fn timeline(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Query(query): Query<TimelineQuery>,
) -> Json<Value> {
    Json(response_data(state.manager.handle_request(
        BridgeRequest::GetTimeline {
            session_id,
            start_ms: query.start_ms,
            end_ms: query.end_ms,
            cursor: query.cursor,
            limit: query.limit,
        },
    )))
}
async fn arm(State(state): State<AppState>, Path(monitor_index): Path<usize>) -> Json<Value> {
    Json(response_data(
        state
            .manager
            .handle_request(BridgeRequest::Arm { monitor_index }),
    ))
}
async fn disarm(State(state): State<AppState>) -> Json<Value> {
    Json(response_data(
        state.manager.handle_request(BridgeRequest::Disarm),
    ))
}
async fn stop(State(state): State<AppState>) -> Json<Value> {
    Json(response_data(
        state.manager.handle_request(BridgeRequest::StopNow),
    ))
}
async fn select_frame(State(state): State<AppState>, Json(body): Json<Selection>) -> Json<Value> {
    Json(response_data(state.manager.handle_request(
        BridgeRequest::SelectFrame {
            session_id: body.session_id,
            offset_ms: body.offset_ms,
        },
    )))
}
async fn shared_frames(State(state): State<AppState>) -> Json<Value> {
    Json(response_data(
        state
            .manager
            .handle_request(BridgeRequest::ListSharedFrames),
    ))
}
async fn remove_shared_frame(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
) -> Json<Value> {
    Json(response_data(state.manager.handle_request(
        BridgeRequest::RemoveSharedFrame { share_id },
    )))
}
async fn clear_shared_frames(State(state): State<AppState>) -> Json<Value> {
    Json(response_data(
        state
            .manager
            .handle_request(BridgeRequest::ClearSharedFrames),
    ))
}
async fn pin(
    State(state): State<AppState>,
    Path((session_id, pinned)): Path<(String, bool)>,
) -> Json<Value> {
    Json(response_data(state.manager.handle_request(
        BridgeRequest::PinSession { session_id, pinned },
    )))
}
async fn delete_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<DeleteRequest>,
) -> Json<Value> {
    Json(response_data(state.manager.handle_request(
        BridgeRequest::DeleteSessionConfirmed {
            session_id,
            delete_pinned: body.delete_pinned,
        },
    )))
}
async fn rename_session(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Json(body): Json<RenameRequest>,
) -> Json<Value> {
    Json(response_data(state.manager.handle_request(
        BridgeRequest::RenameSession {
            session_id,
            display_name: body.display_name,
        },
    )))
}

async fn timeline_map(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Json<Value> {
    Json(
        match state.manager.store().presented_timeline(&session_id) {
            Ok(timeline) => serde_json::to_value(timeline).unwrap_or(Value::Null),
            Err(error) => json!({ "error": error.to_string() }),
        },
    )
}
async fn retention(State(state): State<AppState>, Json(body): Json<Retention>) -> Json<Value> {
    Json(response_data(state.manager.handle_request(
        BridgeRequest::SetRetention { days: body.days },
    )))
}
async fn decrypt_event(
    State(state): State<AppState>,
    Path((session_id, event_id)): Path<(String, i64)>,
) -> Json<Value> {
    Json(
        match state.manager.store().decrypt_event(&session_id, event_id) {
            Ok(value) => json!({ "decrypted": value }),
            Err(error) => json!({ "error": error.to_string() }),
        },
    )
}

async fn media(State(state): State<AppState>, Path(session_id): Path<String>) -> Response {
    let session = match state.manager.store().get_session(&session_id) {
        Ok(session) => session,
        Err(error) => return (StatusCode::NOT_FOUND, error.to_string()).into_response(),
    };
    ServeFile::new(session.media_path)
        .oneshot(Request::new(Body::empty()))
        .await
        .unwrap()
        .into_response()
}

async fn shared_image(State(state): State<AppState>, Path(share_id): Path<String>) -> Response {
    let shared = match state.manager.store().get_shared_frame(&share_id) {
        Ok(shared) => shared,
        Err(error) => return (StatusCode::NOT_FOUND, error.to_string()).into_response(),
    };
    ServeFile::new(shared.image_path)
        .oneshot(Request::new(Body::empty()))
        .await
        .unwrap()
        .into_response()
}

async fn status_partial(State(state): State<AppState>) -> Html<String> {
    let status = state.manager.status();
    let state_name = format!("{:?}", status.state);
    let recording = matches!(status.state, cdxvidext_core::RecorderState::Recording);
    let pulse = if recording { "pulse live" } else { "pulse" };
    let elapsed = status
        .elapsed_ms
        .map(format_duration)
        .unwrap_or_else(|| "00:00.000".into());
    let controls = if recording {
        "<button class=\"button stop\" hx-post=\"/api/stop\" hx-swap=\"none\">Stop recording</button>".into()
    } else if status.armed {
        format!(
            "<button class=\"button danger\" hx-post=\"/api/disarm\" hx-swap=\"none\">Disarm</button>"
        )
    } else {
        let options = status
            .monitors
            .iter()
            .map(|monitor| {
                format!(
                    "<option value=\"{}\">{} · {}×{}{}</option>",
                    monitor.index,
                    escape(&monitor.name),
                    monitor.width,
                    monitor.height,
                    if monitor.primary { " · primary" } else { "" }
                )
            })
            .collect::<String>();
        format!(
            "<select id=\"monitor-select\" aria-label=\"Monitor\">{options}</select><button class=\"button arm\" data-arm>Arm recorder</button>"
        )
    };
    Html(format!(
        r#"<div data-recorder-state="{state_name}"><div class="status-cluster"><span class="{pulse}"></span><div><span class="eyebrow">Recorder state</span><strong>{state_name}</strong></div><time>{elapsed}</time></div><div class="controls">{controls}</div></div>"#
    ))
}

async fn preferences_partial(State(state): State<AppState>) -> Html<String> {
    match state.manager.store().preferences() {
        Ok(preferences) => Html(render_preferences(
            &preferences,
            &state.manager.mcp_status(),
        )),
        Err(error) => Html(format!(
            "<div class=\"preferences-error\">{}</div>",
            escape(&error.to_string())
        )),
    }
}

async fn mcp_status_partial(State(state): State<AppState>) -> Html<String> {
    Html(render_mcp_status(&state.manager.mcp_status()))
}

fn render_preferences(preferences: &Preferences, mcp: &McpConnectionStatus) -> String {
    let cutoff = preferences.cutoff_seconds.unwrap_or(0);
    let cutoff_minutes = cutoff / 60;
    let cutoff_seconds = cutoff % 60;
    let checked = |value: bool| if value { " checked" } else { "" };
    let selected = |value: bool| if value { " checked" } else { "" };
    let disabled = |value: bool| if value { "" } else { " disabled" };
    let control_root = cdxvidext_core::store::data_root()
        .unwrap_or_else(|_| std::path::PathBuf::from(r"%LOCALAPPDATA%\CdxVidExt"));
    let default_flight_root = control_root.join("sessions");
    let default_snapshot_root = control_root.join("exports");
    format!(
        r#"<form id="preferences-form" class="preferences-shell" data-preferences-form>
          <header class="preferences-head"><div><span class="eyebrow">Flight Recorder</span><h2>Preferences</h2></div><button type="button" class="dialog-close" data-close-preferences aria-label="Close preferences">×</button></header>
          <div class="preferences-body">
            <section class="preference-panel wide"><header><span class="eyebrow">Evidence storage</span><h3>Locations</h3></header>
              <div class="preference-row"><label>Recorded flights<small>Complete new flight folders</small></label><output data-path-output="flight">{flight_root}</output><input type="hidden" name="flight_root" value="{flight_root}"><span class="path-actions"><button type="button" class="button mini" data-browse-root="flight">Browse…</button><button type="button" class="text-button" data-reset-root="flight" data-default-root="{default_flight_root}">Default</button></span></div>
              <div class="preference-row"><label>Snapshot images<small>All new extracted PNGs</small></label><output data-path-output="snapshot">{snapshot_root}</output><input type="hidden" name="snapshot_root" value="{snapshot_root}"><span class="path-actions"><button type="button" class="button mini" data-browse-root="snapshot">Browse…</button><button type="button" class="text-button" data-reset-root="snapshot" data-default-root="{default_snapshot_root}">Default</button></span></div>
              <p class="preference-note">Location changes apply only to new evidence. Existing evidence remains indexed in its current folder.</p>
            </section>
            <div class="preferences-grid">
              <section class="preference-panel"><header><span class="eyebrow">Housekeeping</span><h3>Automatic deletion</h3></header>
                <div class="preference-row compact"><label for="flight-retention-enabled">Recorded flights<small>Pinned flights are preserved</small></label><input id="flight-retention-enabled" type="checkbox" name="flight_retention_enabled"{flight_retention_checked}><span class="days-control"><input type="number" name="flight_retention_days" min="1" max="36500" value="{flight_days}"{flight_days_disabled}><span>days</span></span></div>
                <div class="preference-row compact"><label for="snapshot-retention-enabled">Snapshot images<small>Also removes expired tray entries</small></label><input id="snapshot-retention-enabled" type="checkbox" name="snapshot_retention_enabled"{snapshot_retention_checked}><span class="days-control"><input type="number" name="snapshot_retention_days" min="1" max="36500" value="{snapshot_days}"{snapshot_days_disabled}><span>days</span></span></div>
              </section>
              <section class="preference-panel"><header><span class="eyebrow">Capture</span><h3>Recording defaults</h3></header>
                <div class="preference-row compact"><label for="cutoff-enabled">Automatic cutoff<small>Stop normally and remain armed</small></label><input id="cutoff-enabled" type="checkbox" name="cutoff_enabled"{cutoff_checked}><span class="time-control"><input type="number" name="cutoff_minutes" min="0" value="{cutoff_minutes}"{cutoff_disabled}><span>min</span><input type="number" name="cutoff_seconds" min="0" max="59" value="{cutoff_seconds}"{cutoff_disabled}><span>sec</span></span></div>
                <fieldset class="segmented-field"><legend>Quality</legend><label><input type="radio" name="quality" value="low"{quality_low}><span>Low</span></label><label><input type="radio" name="quality" value="medium"{quality_medium}><span>Med</span></label><label><input type="radio" name="quality" value="high"{quality_high}><span>High</span></label></fieldset>
                <fieldset class="segmented-field"><legend>Resolution</legend><label><input type="radio" name="resolution" value="hd1080"{resolution_1080}><span>1080p</span></label><label><input type="radio" name="resolution" value="qhd2k"{resolution_2k}><span>2K</span></label><label><input type="radio" name="resolution" value="native"{resolution_native}><span>Native</span></label></fieldset>
              </section>
            </div>
            <section class="preference-panel wide"><header><span class="eyebrow">Codex integration</span><h3>MCP connection</h3></header><div id="mcp-presence" hx-get="/partials/mcp-status" hx-trigger="every 2s" hx-swap="innerHTML">{mcp_html}</div>
              <details class="mcp-setup"><summary>MCP setup instructions</summary><ol><li>Install and enable the Flight Recorder plugin in Codex.</li><li>Confirm its MCP command points to the packaged bridge executable.</li><li>Restart Codex Desktop after MCP configuration changes.</li><li>Open a new task and ask Codex to open the recorder and show its status.</li></ol></details>
            </section>
          </div>
          <footer class="preferences-foot"><span>Changes are validated before they are applied.</span><p class="preferences-inline-error" data-preferences-error role="alert" tabindex="-1" hidden></p><div><button type="button" class="button" data-close-preferences>Cancel</button><button type="submit" class="button arm">Save preferences</button></div></footer>
        </form>"#,
        flight_root = escape(&preferences.flight_root.to_string_lossy()),
        snapshot_root = escape(&preferences.snapshot_root.to_string_lossy()),
        default_flight_root = escape(&default_flight_root.to_string_lossy()),
        default_snapshot_root = escape(&default_snapshot_root.to_string_lossy()),
        flight_retention_checked = checked(preferences.flight_retention.enabled),
        snapshot_retention_checked = checked(preferences.snapshot_retention.enabled),
        flight_days = preferences.flight_retention.days,
        snapshot_days = preferences.snapshot_retention.days,
        flight_days_disabled = disabled(preferences.flight_retention.enabled),
        snapshot_days_disabled = disabled(preferences.snapshot_retention.enabled),
        cutoff_disabled = disabled(preferences.cutoff_seconds.is_some()),
        cutoff_checked = checked(preferences.cutoff_seconds.is_some()),
        quality_low = selected(matches!(
            preferences.quality,
            cdxvidext_core::CaptureQuality::Low
        )),
        quality_medium = selected(matches!(
            preferences.quality,
            cdxvidext_core::CaptureQuality::Medium
        )),
        quality_high = selected(matches!(
            preferences.quality,
            cdxvidext_core::CaptureQuality::High
        )),
        resolution_1080 = selected(matches!(
            preferences.resolution,
            cdxvidext_core::CaptureResolution::Hd1080
        )),
        resolution_2k = selected(matches!(
            preferences.resolution,
            cdxvidext_core::CaptureResolution::Qhd2k
        )),
        resolution_native = selected(matches!(
            preferences.resolution,
            cdxvidext_core::CaptureResolution::Native
        )),
        mcp_html = render_mcp_status(mcp),
    )
}

fn render_mcp_status(status: &McpConnectionStatus) -> String {
    if let Some(primary) = status.primary.as_ref() {
        format!(
            r#"<div class="mcp-health connected"><strong><span></span>CONNECTED</strong><dl><div><dt>Transport</dt><dd>stdio</dd></div><div><dt>Instances</dt><dd>{instances}</dd></div><div><dt>Bridge</dt><dd>v{version} · PID {pid}</dd></div><div><dt>Started</dt><dd>{started}</dd></div><div><dt>Last seen</dt><dd>{last_seen}</dd></div><div><dt>Executable</dt><dd>{path}</dd></div><div><dt>Recorder</dt><dd>reachable</dd></div></dl></div>"#,
            instances = status.active_instances,
            version = escape(&primary.version),
            pid = primary.pid,
            started = escape(&primary.started_at_utc),
            last_seen = escape(&primary.last_seen_utc),
            path = escape(&primary.executable_path.to_string_lossy()),
        )
    } else {
        "<div class=\"mcp-health disconnected\"><strong><span></span>DISCONNECTED</strong><p>No live Codex-owned MCP bridge heartbeat was detected. Follow the setup instructions below, restart Codex, and open a new task.</p></div>".into()
    }
}

async fn sessions_partial(State(state): State<AppState>) -> Html<String> {
    let (sessions, _) = state
        .manager
        .store()
        .list_sessions(None, 100)
        .unwrap_or_default();
    if sessions.is_empty() {
        return Html("<div class=\"empty-list\"><span>NO FLIGHTS LOGGED</span><p>Arm a monitor, then submit a Codex prompt.</p></div>".into());
    }
    Html(
        sessions
            .into_iter()
            .map(|session| {
                let duration = session
                    .duration_ms
                    .map(format_duration)
                    .unwrap_or_else(|| "—".into());
                let title = session_title(&session);
                let recorded = friendly_recorded_at(&session.started_at_utc);
                let visible_events = state
                    .manager
                    .store()
                    .presented_timeline(&session.session_id)
                    .map(|timeline| timeline.total_events)
                    .unwrap_or(0);
                format!(
                    r##"<article class="session-card" data-flight-card="{id}">
                      <div class="flight-compact">
                        <button class="flight-select" data-select-session="{id}" hx-get="/partials/timeline/{id}" hx-target="#reviewer" hx-swap="innerHTML" aria-label="Open flight {title}"><strong>{title}</strong></button>
                        <div class="flight-quick-actions" aria-label="Actions for {title}"><button class="text-button" data-pin-session="{id}" data-pinned="{pinned}" aria-label="{pin_action} {title}">{pin_action}</button><button class="text-button" data-rename-flight="{id}" aria-label="Rename {title}">Rename</button><button class="text-button danger-text" data-delete-session="{id}" data-pinned="{pinned}" data-flight-title="{title}" aria-label="Delete {title}">Delete</button></div>
                        <button class="flight-duration" data-select-session="{id}" hx-get="/partials/timeline/{id}" hx-target="#reviewer" hx-swap="innerHTML" aria-label="Open flight {title}, duration {duration}"><time>{duration}</time></button>
                        <button class="flight-flip" data-flight-toggle="{id}" aria-expanded="false" aria-label="Show details for {title}"><span>⌄</span></button>
                      </div>
                      <div class="flight-details" data-flight-details="{id}" hidden>
                        <dl><div><dt>Events</dt><dd>{events}</dd></div><div><dt>Recorded</dt><dd>{recorded}</dd></div><div><dt>Display</dt><dd>{monitor}</dd></div><div><dt>Status</dt><dd>{status}</dd></div><div><dt>Pinned</dt><dd>{pinned_label}</dd></div></dl>
                      </div>
                      <div class="rename-flight" data-rename-form="{id}" hidden><input maxlength="80" value="{custom}" aria-label="Recording title"><button class="button mini" data-save-rename="{id}">Save</button><button class="text-button" data-reset-rename="{id}">Reset title</button><button class="text-button" data-cancel-rename="{id}">Cancel</button></div>
                    </article>"##,
                    id = session.session_id,
                    title = escape(&title),
                    duration = duration,
                    events = visible_events,
                    recorded = escape(&recorded),
                    monitor = escape(&format!("Display · {}", session.monitor_name)),
                    status = friendly_session_status(&session.state),
                    pinned_label = if session.pinned { "Yes" } else { "No" },
                    pinned = session.pinned,
                    pin_action = if session.pinned { "Unpin" } else { "Pin" },
                    custom = escape(session.display_name.as_deref().unwrap_or("")),
                )
            })
            .collect(),
    )
}

async fn timeline_partial(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Html<String> {
    let session = match state.manager.store().get_session(&session_id) {
        Ok(session) => session,
        Err(error) => {
            return Html(format!(
                "<div class=\"review-empty\">{}</div>",
                escape(&error.to_string())
            ));
        }
    };
    let title = session_title(&session);
    Html(format!(
        r#"
      <div class="review-head"><div><span class="eyebrow">Selected flight</span><h2>{title}</h2></div><span class="review-duration">{duration_label}</span></div>
      <div class="video-shell" data-session-id="{id}"><video id="flight-video" src="/api/media/{id}" preload="metadata"></video><div class="scanlines"></div></div>
      <div class="transport"><button class="transport-button" data-step="-1">−1f</button><button class="transport-button play" data-play>Play</button><button class="transport-button" data-step="1">+1f</button><div class="timeline-control"><svg id="timeline-markers" viewBox="0 0 1000 32" preserveAspectRatio="none" aria-label="Event markers"></svg><input id="timeline-scrub" type="range" min="0" max="{duration}" value="0"></div><output id="playhead">00:00.000</output></div>
      <section id="telemetry-board" class="telemetry-loading" hx-get="/partials/telemetry/{id}" hx-trigger="load" hx-swap="outerHTML"><span>Decoding observed events…</span></section>
    "#,
        title = escape(&title),
        id = session.session_id,
        duration = session.duration_ms.unwrap_or(0),
        duration_label = session
            .duration_ms
            .map(format_duration)
            .unwrap_or_else(|| "—".into()),
    ))
}

async fn telemetry_partial(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Html<String> {
    let timeline = match state.manager.store().presented_timeline(&session_id) {
        Ok(timeline) => timeline,
        Err(error) => {
            return Html(format!(
                "<section id=\"telemetry-board\" class=\"telemetry-error\">{}</section>",
                escape(&error.to_string())
            ));
        }
    };
    let columns = timeline
        .categories
        .into_iter()
        .map(|category| {
            let category_count = category.events.len();
            let rows = category
                .events
                .into_iter()
                .map(|event| {
                    let time = format_duration(event.end_offset_100ns / 10_000);
                    format!(
                        r#"<article class="telemetry-event" data-event-key="{key}" style="--event-color:{color}"><div class="event-compact"><button class="event-seek" data-event-seek="{seek}" data-event-key="{key}"><strong>#{sequence}</strong><time>{time}</time></button><button class="event-flip" data-event-toggle="{key}" data-event-detail-url="/partials/event/{session}/{key}" aria-expanded="false" aria-label="Show event details">⌄</button></div><div class="event-details" data-event-details="{key}" hidden></div></article>"#,
                        key = escape(&event.event_key),
                        color = escape(&event.color),
                        seek = event.seek_offset_ms,
                        sequence = event.sequence,
                        time = time,
                        session = session_id,
                    )
                })
                .collect::<String>();
            format!(r#"<section class="telemetry-column" data-category="{id}" style="--event-color:{color}"><header><span class="category-swatch"></span><strong>{label}</strong><small>{count}</small></header><div class="telemetry-scroll">{rows}</div></section>"#,
                id=escape(&category.category_id), color=escape(&category.color), label=escape(&category.label), count=category_count, rows=rows)
        })
        .collect::<String>();
    Html(format!(
        r#"<section id="telemetry-board" class="telemetry-board"><header class="telemetry-head"><span class="eyebrow">Observed telemetry</span><strong>{count} navigable events</strong></header><div class="telemetry-columns">{columns}</div></section>"#,
        count = timeline.total_events,
        columns = columns
    ))
}

async fn event_detail_partial(
    State(state): State<AppState>,
    Path((session_id, event_key)): Path<(String, String)>,
) -> Html<String> {
    let detail = match state
        .manager
        .store()
        .presented_event_detail(&session_id, &event_key)
    {
        Ok(detail) => detail,
        Err(error) => {
            return Html(format!(
                "<p class=\"detail-error\">{}</p>",
                escape(&error.to_string())
            ));
        }
    };
    let facts = detail
        .pointer("/event/details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|fact| {
            format!(
                "<div><dt>{}</dt><dd>{}</dd></div>",
                escape(
                    fact.get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("Detail")
                ),
                escape(fact.get("value").and_then(Value::as_str).unwrap_or("—"))
            )
        })
        .collect::<String>();
    let decrypted = match detail.get("decrypted") {
        Some(Value::String(value)) => format!(
            "<div class=\"decrypted-text\"><span>Captured text</span><pre>{}</pre></div>",
            escape(value)
        ),
        Some(value) if !value.is_null() => format!(
            "<div class=\"decrypted-text\"><span>Protected detail</span><pre>{}</pre></div>",
            escape(&value.to_string())
        ),
        _ => String::new(),
    };
    Html(format!(
        "<dl class=\"friendly-facts\">{facts}</dl>{decrypted}"
    ))
}

async fn shared_partial(State(state): State<AppState>) -> Html<String> {
    let frames = match state.manager.store().list_shared_frames() {
        Ok(frames) => frames,
        Err(error) => {
            return Html(format!(
                "<section id=\"share-tray\" class=\"share-tray\"><div class=\"share-error\">{}</div></section>",
                escape(&error.to_string())
            ));
        }
    };
    let count = frames.len();
    let content = if frames.is_empty() {
        "<div class=\"share-empty\"><span>No shared screenshots</span></div>".to_owned()
    } else {
        frames
            .into_iter()
            .map(|frame| {
                let event = frame
                    .nearest_event
                    .as_ref()
                    .map(|item| item.summary.as_str())
                    .unwrap_or("No nearby action");
                let flight_title = state
                    .manager
                    .store()
                    .get_session(&frame.session_id)
                    .map(|session| session_title(&session))
                    .unwrap_or_else(|_| "Deleted flight".into());
                format!(
                    r#"<article class="share-card">
                      <button class="share-preview" data-shared-preview="{share_id}" data-shared-time="{time}" data-shared-session="{session}" data-shared-event="{event}" aria-label="Preview shared frame at {time}">
                        <img src="/api/shared/{share_id}/image" alt="Shared frame at {time}" loading="lazy">
                      </button>
                      <div class="share-meta"><strong>{time}</strong><span>{flight_title}</span></div>
                      <button class="share-remove" data-remove-shared="{share_id}" title="Remove from shared frames" aria-label="Remove shared frame">×</button>
                    </article>"#,
                    share_id = escape(&frame.share_id),
                    time = format_duration(frame.offset_ms.round() as i64),
                    session = escape(&frame.session_id),
                    event = escape(event),
                    flight_title = escape(&flight_title),
                )
            })
            .collect::<String>()
    };
    let clear = if count > 0 {
        "<button class=\"text-button\" data-clear-shared>Clear all</button>"
    } else {
        ""
    };
    Html(format!(
        r#"<section id="share-tray" class="share-dock" hx-get="/partials/shared" hx-trigger="refreshShared" hx-swap="outerHTML">
          <div class="share-count"><strong>{count}</strong><span>shared</span>{clear}</div>
          <div class="share-strip">{content}</div>
        </section>"#
    ))
}

fn response_data(response: cdxvidext_core::model::BridgeResponse) -> Value {
    if response.ok {
        response.data
    } else {
        json!({ "error": response.error })
    }
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn format_duration(ms: i64) -> String {
    let safe = ms.max(0);
    format!(
        "{:02}:{:02}.{:03}",
        safe / 60_000,
        (safe / 1000) % 60,
        safe % 1000
    )
}

fn session_title(session: &cdxvidext_core::SessionSummary) -> String {
    session
        .display_name
        .clone()
        .unwrap_or_else(|| generated_session_title(&session.started_at_utc))
}

fn generated_session_title(started_at: &str) -> String {
    DateTime::parse_from_rfc3339(started_at)
        .map(|value| value.with_timezone(&Local))
        .map(|value| {
            format!(
                "{}_{}_{:02}_{:02}{:02}",
                value.day(),
                value.month(),
                value.year().rem_euclid(100),
                value.hour(),
                value.minute()
            )
        })
        .unwrap_or_else(|_| "Untitled flight".into())
}

fn friendly_recorded_at(started_at: &str) -> String {
    DateTime::parse_from_rfc3339(started_at)
        .map(|value| value.with_timezone(&Local))
        .map(|value| value.format("%B %-d, %Y · %-I:%M %p").to_string())
        .unwrap_or_else(|_| "Recorded time unavailable".into())
}

fn friendly_session_status(state: &str) -> &'static str {
    match state {
        "ready" => "Completed",
        "recording" => "Recording",
        _ => "Interrupted",
    }
}

#[cfg(test)]
mod desktop_tests {
    use super::{
        AppState, compact_window_position, friendly_session_status, generated_session_title,
        normalized_normal_size, render_preferences, routes,
    };
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use cdxvidext_core::{
        BridgeRequest, CaptureQuality, CaptureResolution, McpConnectionStatus, Preferences,
        RecorderManager, RetentionPolicy,
    };
    use std::path::PathBuf;
    use tauri::{PhysicalPosition, PhysicalRect, PhysicalSize};
    use tower::ServiceExt;

    #[tokio::test]
    async fn real_reviewer_requires_session_auth_and_rejects_csrf() {
        let root = std::env::temp_dir().join(format!(
            "Flight Recorder Reviewer Auth {} {}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let manager = RecorderManager::open(root.clone()).expect("open real temporary store");
        let token = "real-test-token";
        let origin = "http://127.0.0.1:43123";
        let app = routes(AppState {
            manager: manager.clone(),
            auth_token: token.into(),
            allowed_origin: origin.into(),
        });

        let unauthenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let bootstrap = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/bootstrap?token={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bootstrap.status(), StatusCode::SEE_OTHER);
        let cookie = bootstrap.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        let session_cookie = cookie.split(';').next().unwrap();

        let authenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authenticated.status(), StatusCode::OK);

        let csrf_rejected = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/consent")
                    .header(header::COOKIE, session_cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(csrf_rejected.status(), StatusCode::FORBIDDEN);

        let accepted = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/consent")
                    .header(header::COOKIE, session_cookie)
                    .header(header::ORIGIN, origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        assert!(manager.handle_request(BridgeRequest::GetPrivacyConsent).ok);

        drop(manager);
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        std::fs::remove_dir_all(root).expect("remove real temporary reviewer store");
    }

    #[test]
    fn raw_session_states_have_friendly_labels() {
        assert_eq!(friendly_session_status("ready"), "Completed");
        assert_eq!(friendly_session_status("recording"), "Recording");
        assert_eq!(friendly_session_status("error"), "Interrupted");
    }

    #[test]
    fn utc_start_time_becomes_the_requested_local_flight_title() {
        assert_eq!(
            generated_session_title("2026-07-18T16:20:00Z"),
            "18_7_26_1220"
        );
    }

    #[test]
    fn compact_window_docks_inside_the_bottom_right_work_area() {
        let work_area = PhysicalRect {
            position: PhysicalPosition::new(-1920, 0),
            size: PhysicalSize::new(1920, 1160),
        };
        assert_eq!(
            compact_window_position(work_area, PhysicalSize::new(636, 82), 12),
            PhysicalPosition::new(-648, 1066)
        );
    }

    #[test]
    fn visible_brand_is_flight_recorder() {
        let index = include_str!("../ui/index.html");
        assert!(index.contains("<title>Flight Recorder</title>"));
        assert!(index.contains("<span class=\"brand-mark\">FR</span>"));
        assert!(index.contains("<b>Flight Recorder</b>"));
        assert!(!index.contains("WINDOWS FLIGHT RECORDER"));
    }

    #[test]
    fn archive_footer_opens_the_real_preferences_dialog() {
        let index = include_str!("../ui/index.html");
        assert!(index.contains("data-open-preferences"));
        assert!(index.contains("id=\"preferences-dialog\""));
        assert!(!index.contains("data-retention"));
    }

    #[test]
    fn collapsed_native_geometry_is_never_restored_as_the_reviewer() {
        let minimum = PhysicalSize::new(1080, 700);
        let fallback = PhysicalSize::new(1380, 880);

        assert_eq!(
            normalized_normal_size(PhysicalSize::new(16, 16), minimum, fallback),
            fallback
        );
        assert_eq!(
            normalized_normal_size(PhysicalSize::new(1440, 900), minimum, fallback),
            PhysicalSize::new(1440, 900)
        );
    }

    #[test]
    fn preferences_partial_contains_every_approved_control_and_live_mcp_state() {
        let preferences = Preferences {
            flight_root: PathBuf::from(r"C:\Evidence\Flights"),
            snapshot_root: PathBuf::from(r"C:\Evidence\Snapshots"),
            flight_retention: RetentionPolicy {
                enabled: true,
                days: 14,
                applies_after_utc: Some("2026-07-19T00:00:00Z".into()),
            },
            snapshot_retention: RetentionPolicy {
                enabled: false,
                days: 30,
                applies_after_utc: None,
            },
            cutoff_seconds: Some(125),
            quality: CaptureQuality::High,
            resolution: CaptureResolution::Qhd2k,
        };
        let html = render_preferences(
            &preferences,
            &McpConnectionStatus {
                connected: false,
                active_instances: 0,
                primary: None,
            },
        );

        for expected in [
            "Recorded flights",
            "Snapshot images",
            "Automatic cutoff",
            "Quality",
            "Resolution",
            "1080p",
            "2K",
            "Native",
            "MCP connection",
            "MCP setup instructions",
            "Save preferences",
            "C:\\Evidence\\Flights",
        ] {
            assert!(html.contains(expected), "missing {expected}");
        }
        assert!(html.contains("value=\"2\""));
        assert!(html.contains("value=\"5\""));
        assert!(html.contains("DISCONNECTED"));
    }
}
