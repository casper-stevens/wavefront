//! Master uplink to a remote relay server (see PROTOCOL.md "Relay extension
//! (v1)"). Instead of serving children directly, the master connects out to
//! a relay's `/source` endpoint, pushes the single captured audio stream to
//! it (same binary frame layout as server.rs), and turns the relay's
//! `child_joined`/`child_left`/`child_latency`/`roster` messages into
//! `AppState::relay_children` entries so the existing dashboard UI just
//! works.

use crate::capture::{AudioChunk, CaptureHandle};
use crate::state::{ClientConfig, ClientKind, Pan, RelayChild, Role, SharedState};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Normalizes a user-entered relay address into:
/// - a public URL to display/share as the join address (e.g.
///   "http://157.173.100.145:8927" or "https://relay.example.com"),
/// - the websocket URL for the `/source` uplink.
///
/// Accepts bare host, "host:port", "http://host[:port]" and
/// "https://host[:port]". A bare host or an "http://" host with no port gets
/// ":8927" appended (the default direct-mode port); "https://" is assumed to
/// go through a reverse proxy on 443 and is left as-is.
pub fn normalize_relay_addr(input: &str) -> (String, String) {
    let trimmed = input.trim().trim_end_matches('/');
    let (scheme, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        ("http", r)
    } else {
        ("http", trimmed)
    };

    let has_port = rest
        .rsplit_once(':')
        .map(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false);

    let host_port = if has_port || scheme == "https" {
        rest.to_string()
    } else {
        format!("{rest}:8927")
    };

    let public_url = format!("{scheme}://{host_port}");
    let ws_scheme = if scheme == "https" { "wss" } else { "ws" };
    let ws_url = format!("{ws_scheme}://{host_port}/source");
    (public_url, ws_url)
}

/// Connects to `relay_url`'s `/source` endpoint and runs the uplink until
/// `stop` is set or the connection drops. Forwards captured audio as binary
/// frames, drains `state.uplink_ctrl` for outgoing `set_config`/
/// `set_crossover` JSON lines, and applies relay->master roster messages to
/// `state.relay_children`.
pub async fn run_uplink(
    relay_url: String,
    state: SharedState,
    capture: CaptureHandle,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let (_public_url, ws_url) = normalize_relay_addr(&relay_url);

    // Auto-reconnect loop: if the master->relay link drops (Wi-Fi blip, relay
    // restart), re-establish it automatically instead of ending the whole host
    // session. `capture` stays alive across attempts and is re-subscribed each
    // time. We do NOT replay missed audio — live playback resyncs to "now" on
    // both ends — we just minimise the outage.
    while !stop.load(Ordering::SeqCst) {
        match connect_and_run(&ws_url, &state, &capture, &stop).await {
            Ok(()) => {}
            Err(e) => eprintln!("wavefront: uplink dropped ({e}); reconnecting…"),
        }
        // Reflect the outage in the UI (roster empties until we're back).
        {
            let mut st = state.lock();
            st.uplink_ctrl = None;
            st.relay_children.clear();
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    }

    {
        let mut st = state.lock();
        st.uplink_ctrl = None;
        st.relay_children.clear();
    }
    // `capture` (owned by this function) is dropped here, stopping the capture
    // thread via CaptureHandle's Drop impl.
    Ok(())
}

/// One connect-and-serve attempt; returns when the link drops or `stop` is set.
async fn connect_and_run(
    ws_url: &str,
    state: &SharedState,
    capture: &CaptureHandle,
    stop: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    let (buffer_ms, crossover_hz) = {
        let st = state.lock();
        (st.buffer_ms, st.crossover_hz)
    };

    let hello = json!({
        "type": "source_hello",
        "buffer_ms": buffer_ms,
        "crossover_hz": crossover_hz,
    });
    write.send(Message::Text(hello.to_string())).await?;

    // Command layer (set_client_config/set_crossover) writes raw JSON lines
    // here; we drain it below and forward each line to the relay.
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<String>();
    state.lock().uplink_ctrl = Some(ctrl_tx);

    let mut audio_rx = capture.subscribe();

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            audio = audio_rx.recv() => {
                match audio {
                    Ok(chunk) => {
                        let frame = encode_audio_frame(&chunk, buffer_ms);
                        if write.send(Message::Binary(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            ctrl = ctrl_rx.recv() => {
                if let Some(text) = ctrl {
                    if write.send(Message::Text(text)).await.is_err() {
                        break;
                    }
                }
            }
            incoming = read.next() => {
                match incoming {
                    Some(Ok(Message::Text(txt))) => handle_relay_text(&txt, &state),
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
        }
    }

    // This attempt ended (drop or stop). Clear the ctrl channel; the outer loop
    // decides whether to reconnect (based on `stop`) and handles roster cleanup.
    state.lock().uplink_ctrl = None;
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RelayToMaster {
    ChildJoined {
        id: u32,
        name: String,
        kind: String,
    },
    ChildLeft {
        id: u32,
    },
    ChildLatency {
        id: u32,
        latency_ms: f64,
    },
    Roster {
        children: Vec<RosterChild>,
    },
}

#[derive(Deserialize)]
struct RosterChild {
    id: u32,
    name: String,
    kind: String,
    latency_ms: f64,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    pan: Option<String>,
    #[serde(default)]
    gain: Option<f32>,
}

fn kind_from_str(k: &str) -> ClientKind {
    if k == "native" {
        ClientKind::Native
    } else {
        ClientKind::Browser
    }
}

fn role_from_str(s: Option<&str>) -> Role {
    match s {
        Some("sub") => Role::Sub,
        Some("tweeter") => Role::Tweeter,
        _ => Role::Full,
    }
}

fn pan_from_str(s: Option<&str>) -> Pan {
    match s {
        Some("left") => Pan::Left,
        Some("right") => Pan::Right,
        _ => Pan::Mid,
    }
}

fn handle_relay_text(txt: &str, state: &SharedState) {
    let Ok(msg) = serde_json::from_str::<RelayToMaster>(txt) else {
        return;
    };
    let mut st = state.lock();
    match msg {
        RelayToMaster::ChildJoined { id, name, kind } => {
            st.relay_children.insert(
                id,
                RelayChild {
                    id,
                    name,
                    kind: kind_from_str(&kind),
                    latency_ms: 0.0,
                    config: ClientConfig::default(),
                },
            );
        }
        RelayToMaster::ChildLeft { id } => {
            st.relay_children.remove(&id);
        }
        RelayToMaster::ChildLatency { id, latency_ms } => {
            if let Some(c) = st.relay_children.get_mut(&id) {
                c.latency_ms = latency_ms;
            }
        }
        RelayToMaster::Roster { children } => {
            let mut map = HashMap::with_capacity(children.len());
            for rc in children {
                let config = ClientConfig {
                    role: role_from_str(rc.role.as_deref()),
                    pan: pan_from_str(rc.pan.as_deref()),
                    gain: rc.gain.unwrap_or(0.8),
                };
                map.insert(
                    rc.id,
                    RelayChild {
                        id: rc.id,
                        name: rc.name,
                        kind: kind_from_str(&rc.kind),
                        latency_ms: rc.latency_ms,
                        config,
                    },
                );
            }
            st.relay_children = map;
        }
    }
}

/// Encodes an audio chunk into the same binary wire frame used by server.rs
/// (PROTOCOL.md's child protocol) — the relay ignores/re-stamps `play_at`.
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
