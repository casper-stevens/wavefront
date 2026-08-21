#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod client;
mod dsp;
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
}

struct AppData {
    state: SharedState,
    runtime: Arc<Runtime>,
}

fn emit_state(app: &AppHandle, shared: &SharedState) {
    let view = shared.lock().to_view();
    let _ = app.emit("wavefront://state", view);
}

fn start_local_playback(runtime: &Arc<Runtime>) {
    let mut guard = runtime.local_playback_stop.lock();
    if guard.is_some() {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    tokio::spawn(async move {
        let _ = client::run_client("127.0.0.1".to_string(), stop_clone, None).await;
    });
    *guard = Some(stop);
}

fn stop_local_playback(runtime: &Arc<Runtime>) {
    if let Some(stop) = runtime.local_playback_stop.lock().take() {
        stop.store(true, Ordering::SeqCst);
    }
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

    let capture_handle = capture::start_capture().map_err(|e| e.to_string())?;
    let source_label = capture_handle.source_label.clone();
    let warnings = capture_handle.warnings.clone();

    {
        let mut st = data.state.lock();
        st.mode = Mode::Master;
        st.capture_source = source_label;
        st.warnings = warnings;
        st.client_addr = None;
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
        start_local_playback(&data.runtime);
    }

    emit_state(&app, &data.state.clone());
    Ok(())
}

#[tauri::command]
async fn stop_master(app: AppHandle, data: State<'_, AppData>) -> Result<(), String> {
    if let Some(handle) = data.runtime.server_task.lock().take() {
        handle.abort();
    }
    stop_local_playback(&data.runtime);

    {
        let mut st = data.state.lock();
        st.mode = Mode::Idle;
        st.clients.clear();
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
        if let Some(c) = st.clients.get_mut(&id) {
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
        for c in st.clients.values() {
            let _ = c.sender.send(ClientMsg::Config(c.config));
        }
    }
    emit_state(&app, &data.state.clone());
    Ok(())
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
    if mode == Mode::Master {
        if on {
            start_local_playback(&data.runtime);
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
            stop_master,
            set_client_config,
            set_crossover,
            set_master_volume,
            set_master_plays,
            start_client,
            stop_client,
            get_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running wavefront");
}
