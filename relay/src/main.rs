//! Wavefront relay: fan-out server for networks that block device-to-device
//! traffic. One master uploads a single audio stream at /source; N children
//! subscribe at /ws and each behaves exactly as if talking to a master. The
//! relay is the clock authority for children and re-stamps every audio chunk
//! on forward. See PROTOCOL.md "Relay extension (v1)".

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};
use tower_http::services::{ServeDir, ServeFile};

const PORT: u16 = 8927;
const DEFAULT_BUFFER_MS: u32 = 1000;

/// One connected child, as tracked by the relay.
struct Child {
    id: u32,
    name: String,
    kind: String, // "native" | "browser"
    latency_ms: f64,
    jitter_ms: f64, // RTT above the child's clean baseline (sync-quality signal)
    suspect: bool,  // the GROUP considers this device a sync outlier
    role: String,   // "sub" | "tweeter" | "full"
    pan: String,    // "left" | "mid" | "right"
    gain: f32,
    /// Text control messages (config/pong) destined for this child.
    ctrl: mpsc::UnboundedSender<String>,
}

impl Child {
    fn config_json(&self, crossover_hz: f32, buffer_ms: u32) -> serde_json::Value {
        json!({
            "type": "config",
            "role": self.role,
            "pan": self.pan,
            "gain": self.gain,
            "crossover_hz": crossover_hz,
            "buffer_ms": buffer_ms,
            "suspect": self.suspect,
        })
    }
}

/// One audio chunk fanned out to children: the PCM payload plus the play time
/// already computed (in relay clock) by the source handler. Computing it once —
/// from the master's timeline — means every child shares the exact same
/// deadline (tighter sync) and chunks burst on reconnect keep their original
/// deadline instead of being re-stamped to "now".
#[derive(Clone)]
struct AudioMsg {
    play_at: f64,
    pcm: Arc<Vec<u8>>,
}

struct Shared {
    start: Instant,
    next_id: AtomicU32,
    children: Mutex<HashMap<u32, Child>>,
    /// Broadcast of (play_at, PCM) chunks from the current source to all child
    /// forwarding tasks.
    audio_tx: broadcast::Sender<AudioMsg>,
    /// Stable master-clock -> relay-clock offset (ms), min-tracked with slow
    /// upward leak. Held across reconnects so a burst of buffered chunks maps to
    /// its correct earlier play time rather than being pushed to "now".
    master_offset: Mutex<Option<f64>>,
    /// Text messages destined for the master's /source socket (child_joined,
    /// child_left, child_latency, roster). None when no master is connected.
    master_ctrl: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// Monotonic token identifying the current source. A newer /source
    /// connection takes over (higher gen); only audio and control from the
    /// current generation are honored, and a stale source disconnecting can
    /// neither clear a newer master's channel nor inject audio. Without this,
    /// any second connection to /source could hijack or kill a live session.
    source_gen: std::sync::atomic::AtomicU64,
    active_source: Mutex<Option<u64>>,
    buffer_ms: Mutex<u32>,
    crossover_hz: Mutex<f32>,
}

impl Shared {
    fn now_ms(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    fn notify_master(&self, msg: serde_json::Value) {
        if let Some(tx) = self.master_ctrl.lock().as_ref() {
            let _ = tx.send(msg.to_string());
        }
    }

    /// Update and return the master->relay clock offset given a fresh candidate
    /// (relay_now - master_ts). Min-tracking: snap down to any tighter (lower)
    /// candidate — the fastest delivery is the truest baseline — but only leak
    /// upward slowly, so occasional late arrivals (and reconnect bursts, which
    /// all look late) don't inflate the baseline and push live audio early.
    /// Flag devices the GROUP considers sync outliers: much jitterier than the
    /// peer median (jitter, not raw latency, is what predicts poor lock). A
    /// flagged child receives suspect=true in its config and force-mutes, so one
    /// device on a bad path can't echo-ruin the room even if it *thinks* it's
    /// fine. Pushes an updated config to any child whose flag changed.
    fn recompute_suspects(&self) {
        let (xover, buf) = (*self.crossover_hz.lock(), *self.buffer_ms.lock());
        let mut children = self.children.lock();
        let mut jitters: Vec<f64> = children
            .values()
            .filter(|c| c.latency_ms > 0.0)
            .map(|c| c.jitter_ms)
            .collect();
        let mut changed: Vec<u32> = Vec::new();
        if jitters.len() < 3 {
            // Too few peers to judge an outlier — clear any flags.
            for c in children.values_mut() {
                if c.suspect {
                    c.suspect = false;
                    changed.push(c.id);
                }
            }
        } else {
            jitters.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = jitters[jitters.len() / 2];
            for c in children.values_mut() {
                if c.latency_ms <= 0.0 {
                    continue;
                }
                let sus = c.jitter_ms > (median * 3.0).max(median + 25.0) && c.jitter_ms > 30.0;
                if sus != c.suspect {
                    c.suspect = sus;
                    changed.push(c.id);
                }
            }
        }
        for id in changed {
            if let Some(c) = children.get(&id) {
                let _ = c.ctrl.send(c.config_json(xover, buf).to_string());
            }
        }
    }

    fn update_master_offset(&self, candidate: f64) -> f64 {
        const LEAK_PER_CHUNK_MS: f64 = 0.05; // ~2.5ms/s: tracks real clock drift
        let mut o = self.master_offset.lock();
        let next = match *o {
            None => candidate,
            Some(cur) if candidate < cur => candidate,
            Some(cur) => cur + (candidate - cur).min(LEAK_PER_CHUNK_MS),
        };
        *o = Some(next);
        next
    }
}

type SharedState = Arc<Shared>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let webclient_dir = resolve_webclient_dir();
    eprintln!("wavefront-relay: serving webclient from {webclient_dir:?}");

    let (audio_tx, _) = broadcast::channel::<AudioMsg>(256);
    let shared: SharedState = Arc::new(Shared {
        start: Instant::now(),
        next_id: AtomicU32::new(1),
        source_gen: std::sync::atomic::AtomicU64::new(0),
        active_source: Mutex::new(None),
        children: Mutex::new(HashMap::new()),
        audio_tx,
        master_offset: Mutex::new(None),
        master_ctrl: Mutex::new(None),
        buffer_ms: Mutex::new(DEFAULT_BUFFER_MS),
        crossover_hz: Mutex::new(220.0),
    });

    // 1 Hz roster to the master.
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tick.tick().await;
                shared.recompute_suspects();
                let children: Vec<serde_json::Value> = shared
                    .children
                    .lock()
                    .values()
                    .map(|c| {
                        json!({
                            "id": c.id, "name": c.name, "kind": c.kind,
                            "latency_ms": c.latency_ms, "jitter_ms": c.jitter_ms,
                            "suspect": c.suspect, "role": c.role,
                            "pan": c.pan, "gain": c.gain,
                        })
                    })
                    .collect();
                shared.notify_master(json!({"type": "roster", "children": children}));
            }
        });
    }

    let index = webclient_dir.join("index.html");
    let serve = ServeDir::new(&webclient_dir).fallback(ServeFile::new(index));

    let app = Router::new()
        .route("/ws", get(child_ws))
        .route("/source", get(source_ws))
        .route("/healthz", get(|| async { "ok" }))
        .fallback_service(serve)
        .with_state(shared);

    let addr = SocketAddr::from(([0, 0, 0, 0], PORT));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("wavefront-relay: listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn resolve_webclient_dir() -> PathBuf {
    // Deployed layout: /opt/wavefront-relay/webclient next to the binary.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let c = dir.join("webclient");
            if c.exists() {
                return c;
            }
        }
    }
    let c = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("webclient");
    if c.exists() {
        return c;
    }
    PathBuf::from("webclient")
}

// ---------------------------------------------------------------------------
// Child subscriber (/ws)
// ---------------------------------------------------------------------------

async fn child_ws(ws: WebSocketUpgrade, State(shared): State<SharedState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| child_conn(socket, shared))
}

async fn child_conn(socket: WebSocket, shared: SharedState) {
    let (mut sender, mut receiver) = socket.split();

    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<String>();
    let mut audio_rx = shared.audio_tx.subscribe();

    // Wait for hello (with a timeout) to learn name/kind.
    let (name, kind) = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        receiver.next(),
    )
    .await
    {
        Ok(Some(Ok(Message::Text(t)))) => {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap_or(json!({}));
            if v.get("type").and_then(|x| x.as_str()) != Some("hello") {
                return;
            }
            (
                v.get("name").and_then(|x| x.as_str()).unwrap_or("Speaker").to_string(),
                v.get("kind").and_then(|x| x.as_str()).unwrap_or("browser").to_string(),
            )
        }
        _ => return,
    };

    let id = shared.next_id.fetch_add(1, Ordering::SeqCst);
    let (buffer_ms, crossover_hz) = (*shared.buffer_ms.lock(), *shared.crossover_hz.lock());

    {
        let mut children = shared.children.lock();
        // Differentiate by default: alternate pan L / R as devices join so no
        // two full-range speakers emit the identical waveform. A timing error
        // between decorrelated signals is far less audible than between
        // identical ones (comb filtering). Role stays "full" — sub/tweeter
        // splitting is the operator's manual tool. `id` is 1-based.
        let default_pan = if id % 2 == 1 { "left" } else { "right" };
        children.insert(
            id,
            Child {
                id,
                name: name.clone(),
                kind: kind.clone(),
                latency_ms: 0.0,
                jitter_ms: 0.0,
                suspect: false,
                role: "full".into(),
                pan: default_pan.into(),
                gain: 0.8,
                ctrl: ctrl_tx.clone(),
            },
        );
    }
    shared.notify_master(json!({"type":"child_joined","id":id,"name":name,"kind":kind}));

    // welcome + initial config
    let _ = sender
        .send(Message::Text(
            json!({"type":"welcome","id":id,"sample_rate":48000,"buffer_ms":buffer_ms})
                .to_string(),
        ))
        .await;
    if let Some(c) = shared.children.lock().get(&id) {
        let _ = ctrl_tx.send(c.config_json(crossover_hz, buffer_ms).to_string());
    }

    // Outbound task: audio + control, multiplexed to this socket. play_at is
    // already computed by the source handler, so no shared state is needed here.
    let out = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                ctrl = ctrl_rx.recv() => {
                    match ctrl {
                        Some(txt) => {
                            if sender.send(Message::Text(txt)).await.is_err() { break; }
                        }
                        None => break,
                    }
                }
                audio = audio_rx.recv() => {
                    match audio {
                        Ok(msg) => {
                            // play_at was computed once by the source handler (relay
                            // clock, from the master timeline) — same for every child.
                            let mut frame = Vec::with_capacity(12 + msg.pcm.len());
                            frame.push(0x01);      // kind = audio
                            frame.push(0);         // flags
                            frame.extend_from_slice(&0u16.to_le_bytes()); // reserved
                            frame.extend_from_slice(&msg.play_at.to_le_bytes());
                            frame.extend_from_slice(&msg.pcm);
                            if sender.send(Message::Binary(frame)).await.is_err() { break; }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }
        }
    });

    // Inbound task: ping (answer with relay clock). Real clients ping every
    // ~2s for clock sync, so a longer silence means the child vanished without
    // a clean close (phone left Wi-Fi, tab killed) — reap it via an idle
    // timeout so it doesn't linger as a ghost speaker in the roster.
    const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    loop {
        let msg = match tokio::time::timeout(IDLE_TIMEOUT, receiver.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => break, // timeout, stream end, or error → gone
        };
        match msg {
            Message::Text(t) => {
                let v: serde_json::Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v.get("type").and_then(|x| x.as_str()) == Some("ping") {
                    let t0 = v.get("t0").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let t1 = shared.now_ms();
                    let _ = ctrl_tx.send(json!({"type":"pong","t0":t0,"t1":t1}).to_string());
                    // The child reports its own measured RTT; store it so the
                    // roster carries real per-device latency to the master.
                    if let Some(rtt) = v.get("rtt").and_then(|x| x.as_f64()) {
                        if let Some(c) = shared.children.lock().get_mut(&id) {
                            c.latency_ms = rtt;
                            if let Some(j) = v.get("jitter").and_then(|x| x.as_f64()) {
                                c.jitter_ms = j;
                            }
                        }
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    out.abort();
    shared.children.lock().remove(&id);
    shared.notify_master(json!({"type":"child_left","id":id}));
}

// ---------------------------------------------------------------------------
// Master uplink (/source)
// ---------------------------------------------------------------------------

async fn source_ws(ws: WebSocketUpgrade, State(shared): State<SharedState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| source_conn(socket, shared))
}

async fn source_conn(socket: WebSocket, shared: SharedState) {
    let (mut sender, mut receiver) = socket.split();

    // Claim source ownership. A newer connection wins; this generation gates
    // both audio forwarding and the final channel teardown so a stale or
    // rogue /source can't hijack or kill the live master.
    let my_gen = shared.source_gen.fetch_add(1, Ordering::SeqCst) + 1;
    *shared.active_source.lock() = Some(my_gen);

    // Register this master's control channel so the relay can push roster etc.
    let (m_tx, mut m_rx) = mpsc::unbounded_channel::<String>();
    *shared.master_ctrl.lock() = Some(m_tx);

    // Push current roster immediately so the reconnecting master resyncs.
    {
        let children: Vec<serde_json::Value> = shared
            .children
            .lock()
            .values()
            .map(|c| {
                json!({"id":c.id,"name":c.name,"kind":c.kind,"latency_ms":c.latency_ms,
                       "jitter_ms":c.jitter_ms,"suspect":c.suspect,
                       "role":c.role,"pan":c.pan,"gain":c.gain})
            })
            .collect();
        shared.notify_master(json!({"type":"roster","children":children}));
    }

    let out = tokio::spawn(async move {
        while let Some(txt) = m_rx.recv().await {
            if sender.send(Message::Text(txt)).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = receiver.next().await {
        // If a newer source took over, this one is stale — stop immediately.
        if *shared.active_source.lock() != Some(my_gen) {
            break;
        }
        match msg {
            Message::Binary(bin) => {
                if bin.len() > 12 && bin[0] == 0x01 {
                    // The master stamps capture-time (its own clock). Translate it
                    // into relay clock via a STABLE offset so the buffer lead is
                    // preserved end-to-end and reconnect bursts keep their deadline.
                    let master_ts = f64::from_le_bytes(bin[4..12].try_into().unwrap());
                    let buffer_ms = *shared.buffer_ms.lock() as f64;
                    let offset = shared.update_master_offset(shared.now_ms() - master_ts);
                    let play_at = master_ts + offset + buffer_ms;
                    let pcm = Arc::new(bin[12..].to_vec());
                    let _ = shared.audio_tx.send(AudioMsg { play_at, pcm });
                }
            }
            Message::Text(t) => handle_source_text(&t, &shared),
            Message::Close(_) => break,
            _ => {}
        }
    }

    out.abort();
    // Only tear down the shared channel if we're still the current owner; a
    // stale source that lost the race must not clear a newer master.
    let mut active = shared.active_source.lock();
    if *active == Some(my_gen) {
        *active = None;
        *shared.master_ctrl.lock() = None;
    }
}

fn handle_source_text(t: &str, shared: &SharedState) {
    let v: serde_json::Value = match serde_json::from_str(t) {
        Ok(v) => v,
        Err(_) => return,
    };
    match v.get("type").and_then(|x| x.as_str()) {
        Some("source_hello") => {
            if let Some(b) = v.get("buffer_ms").and_then(|x| x.as_u64()) {
                *shared.buffer_ms.lock() = b as u32;
            }
            if let Some(hz) = v.get("crossover_hz").and_then(|x| x.as_f64()) {
                *shared.crossover_hz.lock() = hz as f32;
            }
        }
        Some("set_config") => {
            let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let (crossover_hz, buffer_ms) = (*shared.crossover_hz.lock(), *shared.buffer_ms.lock());
            let mut children = shared.children.lock();
            if let Some(c) = children.get_mut(&id) {
                if let Some(r) = v.get("role").and_then(|x| x.as_str()) {
                    c.role = r.to_string();
                }
                if let Some(p) = v.get("pan").and_then(|x| x.as_str()) {
                    c.pan = p.to_string();
                }
                if let Some(g) = v.get("gain").and_then(|x| x.as_f64()) {
                    c.gain = g as f32;
                }
                let _ = c.ctrl.send(c.config_json(crossover_hz, buffer_ms).to_string());
            }
        }
        Some("set_crossover") => {
            if let Some(hz) = v.get("hz").and_then(|x| x.as_f64()) {
                *shared.crossover_hz.lock() = hz as f32;
            }
            let (crossover_hz, buffer_ms) = (*shared.crossover_hz.lock(), *shared.buffer_ms.lock());
            for c in shared.children.lock().values() {
                let _ = c.ctrl.send(c.config_json(crossover_hz, buffer_ms).to_string());
            }
        }
        _ => {}
    }
}

