#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod client;
mod dsp;
mod relay_uplink;
mod server;
mod state;

use state::{ClientConfig, ClientMsg, Mode, Pan, Role, SharedState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};
use tokio::task::JoinHandle;

/// Backend-only orchestration handles, not part of the serialized state.
#[derive(Default)]
struct Runtime {
    server_task: parking_lot::Mutex<Option<JoinHandle<()>>>,
    local_playback_stop: parking_lot::Mutex<Option<Arc<AtomicBool>>>,
    client_task: parking_lot::Mutex<Option<JoinHandle<()>>>,
    client_stop: parking_lot::Mutex<Option<Arc<AtomicBool>>>,
    // Relay uplink (RelayHost mode). The outgoing control sender itself
    // lives on AppState::uplink_ctrl (state.rs), since the command layer
    // only has access to `data.state`, not `data.runtime`, when routing
    // set_client_config/set_crossover.
    uplink_task: parking_lot::Mutex<Option<JoinHandle<()>>>,
    uplink_stop: parking_lot::Mutex<Option<Arc<AtomicBool>>>,
}

struct AppData {
    state: SharedState,
    runtime: Arc<Runtime>,
}

fn emit_state(app: &AppHandle, shared: &SharedState) {
    let view = shared.lock().to_view();
    let _ = app.emit("wavefront://state", view);
}

fn start_local_playback(runtime: &Arc<Runtime>, state: &SharedState) {
    let mut guard = runtime.local_playback_stop.lock();
    if guard.is_some() {
        return;
    }
    // In RelayHost mode the master has no local server to loop back to, so
    // point its own playback pipeline at the relay instead, same as any
    // other child. client.rs only ever dials `ws://`, so this only works
    // for a plain host/http relay address, not an https(wss) one.
    let target = {
        let st = state.lock();
        if st.mode == Mode::RelayHost {
            st.relay_url
                .as_deref()
                .map(|u| {
                    u.trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .to_string()
                })
                .unwrap_or_else(|| "127.0.0.1".to_string())
        } else {
            "127.0.0.1".to_string()
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    tokio::spawn(async move {
        let _ = client::run_client(target, stop_clone, None).await;
    });
    *guard = Some(stop);
}

fn stop_local_playback(runtime: &Arc<Runtime>) {
    if let Some(stop) = runtime.local_playback_stop.lock().take() {
        stop.store(true, Ordering::SeqCst);
    }
}

/// Stops whatever hosting pipeline (direct server or relay uplink) is
/// currently running, plus the master's own local playback loop.
fn stop_hosting(runtime: &Arc<Runtime>) {
    if let Some(handle) = runtime.server_task.lock().take() {
        handle.abort();
    }
    if let Some(handle) = runtime.uplink_task.lock().take() {
        handle.abort();
    }
    if let Some(stop) = runtime.uplink_stop.lock().take() {
        stop.store(true, Ordering::SeqCst);
    }
    stop_local_playback(runtime);
}

#[tauri::command]
async fn start_master(
    app: AppHandle,
    data: State<'_, AppData>,
) -> Result<(), String> {
    let already_master = { data.state.lock().mode == Mode::Master };
    if already_master {
        return Ok(());
    }
    stop_hosting(&data.runtime);

    let capture_handle = capture::start_capture().map_err(|e| e.to_string())?;
    let source_label = capture_handle.source_label.clone();
    let warnings = capture_handle.warnings.clone();

    {
        let mut st = data.state.lock();
        st.mode = Mode::Master;
        st.capture_source = source_label;
        st.warnings = warnings;
        st.client_addr = None;
        st.relay_url = None;
        st.relay_children.clear();
        st.uplink_ctrl = None;
    }

    let app_for_server = app.clone();
    let shared_for_server = data.state.clone();
    // capture_handle is moved into this task; dropping/aborting the task stops
    // the underlying capture thread (see CaptureHandle's Drop impl).
    let handle = tokio::spawn(async move {
        if let Err(e) = server::run_server(app_for_server, shared_for_server, capture_handle).await {
            eprintln!("wavefront: server error: {e}");
        }
    });
    *data.runtime.server_task.lock() = Some(handle);

    if data.state.lock().master_plays {
        start_local_playback(&data.runtime, &data.state);
    }

    emit_state(&app, &data.state.clone());
    Ok(())
}

/// Like `start_master`, but pushes the captured stream to a remote relay
/// (see relay_uplink.rs / PROTOCOL.md's "Relay extension (v1)") instead of
/// serving children directly — for networks that block device-to-device
/// traffic.
#[tauri::command]
async fn start_relay_host(
    app: AppHandle,
    data: State<'_, AppData>,
    relay: String,
) -> Result<(), String> {
    let already_relay = { data.state.lock().mode == Mode::RelayHost };
    if already_relay {
        return Ok(());
    }
    stop_hosting(&data.runtime);

    let (public_url, _ws_url) = relay_uplink::normalize_relay_addr(&relay);

    let capture_handle = capture::start_capture().map_err(|e| e.to_string())?;
    let source_label = capture_handle.source_label.clone();
    let warnings = capture_handle.warnings.clone();

    {
        let mut st = data.state.lock();
        st.mode = Mode::RelayHost;
        st.capture_source = source_label;
        st.warnings = warnings;
        st.client_addr = None;
        st.clients.clear();
        st.relay_children.clear();
        st.relay_url = Some(public_url);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let state_for_uplink = data.state.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) =
            relay_uplink::run_uplink(relay, state_for_uplink, capture_handle, stop_clone).await
        {
            eprintln!("wavefront: relay uplink error: {e}");
        }
    });
    *data.runtime.uplink_task.lock() = Some(handle);
    *data.runtime.uplink_stop.lock() = Some(stop);

    if data.state.lock().master_plays {
        start_local_playback(&data.runtime, &data.state);
    }

    emit_state(&app, &data.state.clone());
    Ok(())
}

#[tauri::command]
async fn stop_master(app: AppHandle, data: State<'_, AppData>) -> Result<(), String> {
    stop_hosting(&data.runtime);

    {
        let mut st = data.state.lock();
        st.mode = Mode::Idle;
        st.clients.clear();
        st.relay_children.clear();
        st.relay_url = None;
        st.uplink_ctrl = None;
        st.capture_source.clear();
        st.warnings.clear();
    }
    emit_state(&app, &data.state.clone());
    Ok(())
}

#[tauri::command]
async fn set_client_config(
    app: AppHandle,
    data: State<'_, AppData>,
    id: u32,
    role: Role,
    pan: Pan,
    gain: f32,
) -> Result<(), String> {
    let cfg = ClientConfig { role, pan, gain };
    {
        let mut st = data.state.lock();
        if st.mode == Mode::RelayHost {
            if let Some(c) = st.relay_children.get_mut(&id) {
                c.config = cfg;
            }
            if let Some(tx) = &st.uplink_ctrl {
                let msg = serde_json::json!({
                    "type": "set_config",
                    "id": id,
                    "role": role_str(role),
                    "pan": pan_str(pan),
                    "gain": gain,
                });
                let _ = tx.send(msg.to_string());
            }
        } else if let Some(c) = st.clients.get_mut(&id) {
            c.config = cfg;
            let _ = c.sender.send(ClientMsg::Config(cfg));
        }
    }
    emit_state(&app, &data.state.clone());
    Ok(())
}

#[tauri::command]
async fn set_crossover(app: AppHandle, data: State<'_, AppData>, hz: f32) -> Result<(), String> {
    {
        let mut st = data.state.lock();
        st.crossover_hz = hz;
        if st.mode == Mode::RelayHost {
            if let Some(tx) = &st.uplink_ctrl {
                let msg = serde_json::json!({"type": "set_crossover", "hz": hz});
                let _ = tx.send(msg.to_string());
            }
        } else {
            for c in st.clients.values() {
                let _ = c.sender.send(ClientMsg::Config(c.config));
            }
        }
    }
    emit_state(&app, &data.state.clone());
    Ok(())
}

#[tauri::command]
async fn set_buffer(app: AppHandle, data: State<'_, AppData>, ms: u32) -> Result<(), String> {
    let ms = ms.clamp(100, 3000);
    {
        let mut st = data.state.lock();
        st.buffer_ms = ms;
        if st.mode == Mode::RelayHost {
            if let Some(tx) = &st.uplink_ctrl {
                let msg = serde_json::json!({"type": "set_buffer", "ms": ms});
                let _ = tx.send(msg.to_string());
            }
        } else {
            for c in st.clients.values() {
                let _ = c.sender.send(ClientMsg::Config(c.config));
            }
        }
    }
    emit_state(&app, &data.state.clone());
    Ok(())
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Sub => "sub",
        Role::Tweeter => "tweeter",
        Role::Full => "full",
    }
}

fn pan_str(pan: Pan) -> &'static str {
    match pan {
        Pan::Left => "left",
        Pan::Mid => "mid",
        Pan::Right => "right",
    }
}

#[tauri::command]
async fn set_master_volume(app: AppHandle, data: State<'_, AppData>, v: f32) -> Result<(), String> {
    {
        let mut st = data.state.lock();
        st.master_volume = v.clamp(0.0, 1.0);
    }
    emit_state(&app, &data.state.clone());
    Ok(())
}

#[tauri::command]
async fn set_master_plays(app: AppHandle, data: State<'_, AppData>, on: bool) -> Result<(), String> {
    let mode = {
        let mut st = data.state.lock();
        st.master_plays = on;
        st.mode
    };
    if mode == Mode::Master || mode == Mode::RelayHost {
        if on {
            start_local_playback(&data.runtime, &data.state);
        } else {
            stop_local_playback(&data.runtime);
        }
    }
    emit_state(&app, &data.state.clone());
    Ok(())
}

#[tauri::command]
async fn start_client(app: AppHandle, data: State<'_, AppData>, addr: String) -> Result<(), String> {
    if let Some(handle) = data.runtime.client_task.lock().take() {
        handle.abort();
    }
    if let Some(stop) = data.runtime.client_stop.lock().take() {
        stop.store(true, Ordering::SeqCst);
    }

    {
        let mut st = data.state.lock();
        st.mode = Mode::Client;
        st.client_addr = Some(addr.clone());
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let addr_clone = addr.clone();
    let state_for_client = data.state.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = client::run_client(addr_clone, stop_clone, Some(state_for_client)).await {
            eprintln!("wavefront: client error: {e}");
        }
    });
    *data.runtime.client_task.lock() = Some(handle);
    *data.runtime.client_stop.lock() = Some(stop);

    emit_state(&app, &data.state.clone());
    Ok(())
}

#[tauri::command]
async fn stop_client(app: AppHandle, data: State<'_, AppData>) -> Result<(), String> {
    if let Some(handle) = data.runtime.client_task.lock().take() {
        handle.abort();
    }
    if let Some(stop) = data.runtime.client_stop.lock().take() {
        stop.store(true, Ordering::SeqCst);
    }
    {
        let mut st = data.state.lock();
        st.mode = Mode::Idle;
        st.client_addr = None;
        st.client_status = None;
    }
    emit_state(&app, &data.state.clone());
    Ok(())
}

#[tauri::command]
async fn get_status(data: State<'_, AppData>) -> Result<state::StateView, String> {
    Ok(data.state.lock().to_view())
}

fn main() {
    let shared_state: SharedState = Arc::new(parking_lot::Mutex::new(state::AppState::default()));
    let runtime = Arc::new(Runtime::default());

    tauri::Builder::default()
        .manage(AppData {
            state: shared_state.clone(),
            runtime,
        })
        .setup(move |app| {
            let app_handle = app.handle().clone();
            let state_for_tick = shared_state.clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
                loop {
                    interval.tick().await;
                    emit_state(&app_handle, &state_for_tick);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_master,
            start_relay_host,
            stop_master,
            set_client_config,
            set_crossover,
            set_buffer,
            set_master_volume,
            set_master_plays,
            start_client,
            stop_client,
            get_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running wavefront");
}
