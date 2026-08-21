//! Axum server: serves the browser webclient and handles the /ws protocol
//! described in PROTOCOL.md.

use crate::capture::{AudioChunk, CaptureHandle};
use crate::state::{ClientConfig, ClientEntry, ClientKind, ClientMsg, SharedState};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tower_http::services::{ServeDir, ServeFile};

pub const PORT: u16 = 8927;

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

#[derive(Clone)]
struct ServerCtx {
    state: SharedState,
    capture: std::sync::Arc<CaptureHandle>,
}

/// Resolves the directory containing the static browser webclient assets.
/// Prefers a Tauri-resolved resource dir; falls back to `./webclient`
/// relative to the current working directory (useful in dev).
fn resolve_webclient_dir(app: &tauri::AppHandle) -> PathBuf {
    use tauri::Manager;
    if let Ok(resource_dir) = app.path().resource_dir() {
        let candidate = resource_dir.join("webclient");
        if candidate.exists() {
            return candidate;
        }
    }
    // Dev fallbacks: `cargo tauri dev` runs with cwd = src-tauri, so the
    // repo's webclient/ sits one level up.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let local = cwd.join("webclient");
    if local.exists() {
        return local;
    }
    cwd.join("../webclient")
}

pub async fn run_server(
    app: tauri::AppHandle,
    state: SharedState,
    capture: CaptureHandle,
) -> anyhow::Result<()> {
    let webclient_dir = resolve_webclient_dir(&app);
    let capture = std::sync::Arc::new(capture);
    let ctx = ServerCtx {
        state,
        capture,
    };

    let index_file = webclient_dir.join("index.html");
    let serve_dir = ServeDir::new(&webclient_dir).fallback(ServeFile::new(index_file));

    let router = Router::new()
        .route("/ws", get(ws_handler))
        .fallback_service(serve_dir)
        .with_state(ctx);

    let addr = SocketAddr::from(([0, 0, 0, 0], PORT));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(ctx): State<ServerCtx>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, ctx))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientToMaster {
    Hello { name: String, kind: String },
    Ping { t0: f64 },
}

async fn handle_socket(mut socket: WebSocket, ctx: ServerCtx) {
    // Wait for the hello message first.
    let hello = loop {
        match socket.recv().await {
            Some(Ok(Message::Text(txt))) => match serde_json::from_str::<ClientToMaster>(&txt) {
                Ok(ClientToMaster::Hello { name, kind }) => break (name, kind),
                _ => continue,
            },
            Some(Ok(_)) => continue,
            _ => return,
        }
    };

    let (name, kind_str) = hello;
    let kind = if kind_str == "native" {
        ClientKind::Native
    } else {
        ClientKind::Browser
    };

    let (tx, mut ctrl_rx) = mpsc::unbounded_channel::<ClientMsg>();
    let default_config = ClientConfig::default();

    let (id, buffer_ms, crossover_hz) = {
        let mut st = ctx.state.lock();
        let id = st.next_client_id;
        st.next_client_id += 1;
        st.clients.insert(
            id,
            ClientEntry {
                id,
                name: name.clone(),
                kind,
                latency_ms: 0.0,
                config: default_config,
                sender: tx,
            },
        );
        (id, st.buffer_ms, st.crossover_hz)
    };

    let welcome = json!({
        "type": "welcome",
        "id": id,
        "sample_rate": 48000,
        "buffer_ms": buffer_ms,
    });
    if socket
        .send(Message::Text(welcome.to_string()))
        .await
        .is_err()
    {
        deregister(&ctx.state, id);
        return;
    }

    let config_msg = config_to_json(&default_config, crossover_hz, buffer_ms);
    if socket
        .send(Message::Text(config_msg.to_string()))
        .await
        .is_err()
    {
        deregister(&ctx.state, id);
        return;
    }

    let mut audio_rx = ctx.capture.subscribe();
    let buffer_ms_atomic = AtomicU64::new(buffer_ms as u64);

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(ClientToMaster::Ping { t0 }) = serde_json::from_str::<ClientToMaster>(&txt) {
                            let t1 = now_ms();
                            let rtt_half = (t1 - t0).max(0.0) / 2.0;
                            {
                                let mut st = ctx.state.lock();
                                if let Some(c) = st.clients.get_mut(&id) {
                                    c.latency_ms = rtt_half;
                                }
                            }
                            let pong = json!({"type": "pong", "t0": t0, "t1": t1});
                            if socket.send(Message::Text(pong.to_string())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            ctrl = ctrl_rx.recv() => {
                match ctrl {
                    Some(ClientMsg::Config(cfg)) => {
                        let bm = buffer_ms_atomic.load(Ordering::Relaxed) as u32;
                        let xover = ctx.state.lock().crossover_hz;
                        let msg = config_to_json(&cfg, xover, bm);
                        if socket.send(Message::Text(msg.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Some(ClientMsg::Disconnect) | None => break,
                }
            }
            audio = audio_rx.recv() => {
                match audio {
                    Ok(mut chunk) => {
                        let (bm, volume) = {
                            let st = ctx.state.lock();
                            (st.buffer_ms, st.master_volume)
                        };
                        if (volume - 1.0).abs() > f32::EPSILON {
                            for s in chunk.pcm.iter_mut() {
                                *s = (*s as f32 * volume).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                            }
                        }
                        let frame = encode_audio_frame(&chunk, bm);
                        if socket.send(Message::Binary(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => {}
                }
            }
        }
    }

    deregister(&ctx.state, id);
}

fn deregister(state: &SharedState, id: u32) {
    state.lock().clients.remove(&id);
}

fn config_to_json(cfg: &ClientConfig, crossover_hz: f32, buffer_ms: u32) -> serde_json::Value {
    json!({
        "type": "config",
        "role": role_str(cfg.role),
        "pan": pan_str(cfg.pan),
        "gain": cfg.gain,
        "crossover_hz": crossover_hz,
        "buffer_ms": buffer_ms,
    })
}

fn role_str(role: crate::state::Role) -> &'static str {
    match role {
        crate::state::Role::Sub => "sub",
        crate::state::Role::Tweeter => "tweeter",
        crate::state::Role::Full => "full",
    }
}

fn pan_str(pan: crate::state::Pan) -> &'static str {
    match pan {
        crate::state::Pan::Left => "left",
        crate::state::Pan::Mid => "mid",
        crate::state::Pan::Right => "right",
    }
}

/// Encodes an audio chunk into the binary wire frame described in PROTOCOL.md.
fn encode_audio_frame(chunk: &AudioChunk, buffer_ms: u32) -> Vec<u8> {
    let play_at = chunk.capture_ts_ms + buffer_ms as f64;
    let mut buf = Vec::with_capacity(12 + chunk.pcm.len() * 2);
    buf.push(0x01u8); // frame kind = audio
    buf.push(0u8); // flags
    buf.extend_from_slice(&0u16.to_le_bytes()); // reserved
    buf.extend_from_slice(&play_at.to_le_bytes());
    for s in &chunk.pcm {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    buf
}
