//! Native client mode: connects to a master's /ws endpoint, keeps clock
//! offset via a ping loop, buffers incoming audio chunks by play time, and
//! feeds a cpal output stream applying per-client DSP.

use crate::dsp::DspChain;
use crate::server::PORT;
use crate::state::{ClientConfig, ClientStatus, SharedState};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio_tungstenite::tungstenite::Message;

fn wall_now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

struct QueuedChunk {
    play_at_local_ms: f64,
    pcm: Vec<i16>,
}

impl PartialEq for QueuedChunk {
    fn eq(&self, other: &Self) -> bool {
        self.play_at_local_ms == other.play_at_local_ms
    }
}
impl Eq for QueuedChunk {}
impl PartialOrd for QueuedChunk {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.play_at_local_ms.partial_cmp(&other.play_at_local_ms)
    }
}
impl Ord for QueuedChunk {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// Shared runtime state between the websocket task and the cpal output thread.
struct ClientShared {
    queue: Mutex<BinaryHeap<Reverse<QueuedChunk>>>,
    dsp: Mutex<DspChain>,
    config: Mutex<ClientConfig>,
    offset_ms: Mutex<f64>,
    offset_samples: Mutex<VecDeque<f64>>,
}

fn local_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "wavefront-client".to_string())
}

/// Connects to `addr` (host:port or host, defaults to PORT) and runs the full
/// client pipeline until `stop` is set. Runs the websocket I/O on the current
/// tokio task and the cpal output stream on a dedicated std thread.
///
/// When `report` is Some, live status (connection, received config, RTT) is
/// written into the shared app state for the UI. The master's own local
/// playback pipeline passes None so it doesn't masquerade as client mode.
pub async fn run_client(
    addr: String,
    stop: Arc<AtomicBool>,
    report: Option<SharedState>,
) -> anyhow::Result<()> {
    let target = if addr.contains(':') {
        addr.clone()
    } else {
        format!("{addr}:{PORT}")
    };
    let url = format!("ws://{target}/ws");

    let set_status = |f: &dyn Fn(&mut ClientStatus)| {
        if let Some(st) = &report {
            let mut guard = st.lock();
            let status = guard.client_status.get_or_insert_with(ClientStatus::default);
            f(status);
        }
    };

    let connect_result = tokio_tungstenite::connect_async(&url).await;
    let (ws_stream, _) = match connect_result {
        Ok(ok) => {
            set_status(&|s| {
                s.connected = true;
                s.master_addr = addr.clone();
            });
            ok
        }
        Err(e) => {
            set_status(&|s| s.connected = false);
            return Err(e.into());
        }
    };
    let (mut write, mut read) = ws_stream.split();

    let hello = serde_json::json!({
        "type": "hello",
        "name": local_hostname(),
        "kind": "native",
    });
    write.send(Message::Text(hello.to_string())).await?;

    let shared = Arc::new(ClientShared {
        queue: Mutex::new(BinaryHeap::new()),
        dsp: Mutex::new(DspChain::new(ClientConfig::default(), 220.0)),
        config: Mutex::new(ClientConfig::default()),
        offset_ms: Mutex::new(0.0),
        offset_samples: Mutex::new(VecDeque::with_capacity(15)),
    });

    // Spawn the cpal output stream on its own thread (cpal streams are !Send).
    let output_stop = stop.clone();
    let output_shared = shared.clone();
    std::thread::spawn(move || run_output_stream(output_shared, output_stop));

    let start = Instant::now();

    // Ping loop task.
    {
        let stop = stop.clone();
        let mut write_for_ping = write;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let t0 = wall_now_ms();
                let ping = serde_json::json!({"type": "ping", "t0": t0});
                if write_for_ping
                    .send(Message::Text(ping.to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    while !stop.load(Ordering::SeqCst) {
        let msg = tokio::select! {
            m = read.next() => m,
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => continue,
        };
        let msg = match msg {
            Some(Ok(m)) => m,
            Some(Err(_)) | None => break,
        };
        match msg {
            Message::Text(txt) => handle_text(&txt, &shared, &report),
            Message::Binary(bin) => handle_binary(&bin, &shared, &start),
            Message::Close(_) => break,
            _ => {}
        }
    }

    set_status(&|s| s.connected = false);
    stop.store(true, Ordering::SeqCst);
    Ok(())
}

fn handle_text(txt: &str, shared: &Arc<ClientShared>, report: &Option<SharedState>) {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(txt) else {
        return;
    };
    match val.get("type").and_then(|v| v.as_str()) {
        Some("pong") => {
            let t0 = val.get("t0").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let t1 = val.get("t1").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let t_recv = wall_now_ms();
            let rtt = (t_recv - t0).max(0.0);
            let offset = t1 + rtt / 2.0 - t_recv;

            let mut samples = shared.offset_samples.lock();
            if samples.len() >= 15 {
                samples.pop_front();
            }
            samples.push_back(offset);
            let mut sorted: Vec<f64> = samples.iter().copied().collect();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median = sorted[sorted.len() / 2];
            *shared.offset_ms.lock() = median;

            if let Some(st) = report {
                let mut guard = st.lock();
                if let Some(status) = guard.client_status.as_mut() {
                    status.latency_ms = rtt;
                }
            }
        }
        Some("config") => {
            let role = match val.get("role").and_then(|v| v.as_str()) {
                Some("sub") => crate::state::Role::Sub,
                Some("tweeter") => crate::state::Role::Tweeter,
                _ => crate::state::Role::Full,
            };
            let pan = match val.get("pan").and_then(|v| v.as_str()) {
                Some("left") => crate::state::Pan::Left,
                Some("right") => crate::state::Pan::Right,
                _ => crate::state::Pan::Mid,
            };
            let gain = val.get("gain").and_then(|v| v.as_f64()).unwrap_or(0.8) as f32;
            let crossover_hz = val
                .get("crossover_hz")
                .and_then(|v| v.as_f64())
                .unwrap_or(220.0) as f32;
            let cfg = ClientConfig { role, pan, gain };
            *shared.config.lock() = cfg;
            shared.dsp.lock().set_config(cfg, crossover_hz);

            if let Some(st) = report {
                let mut guard = st.lock();
                if let Some(status) = guard.client_status.as_mut() {
                    status.role = role;
                    status.pan = pan;
                    status.gain = gain;
                    status.crossover_hz = crossover_hz;
                }
            }
        }
        _ => {}
    }
}

fn handle_binary(bin: &[u8], shared: &Arc<ClientShared>, _start: &Instant) {
    if bin.len() < 12 || bin[0] != 0x01 {
        return;
    }
    let play_at = f64::from_le_bytes(bin[4..12].try_into().unwrap());
    let pcm_bytes = &bin[12..];
    let pcm: Vec<i16> = pcm_bytes
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();

    let offset = *shared.offset_ms.lock();
    let local_play_at = play_at - offset;

    // Drop chunks whose play time has already passed.
    let now = wall_now_ms();
    if local_play_at < now {
        return;
    }

    shared
        .queue
        .lock()
        .push(Reverse(QueuedChunk {
            play_at_local_ms: local_play_at,
            pcm,
        }));
}

/// Runs the cpal output stream on the calling (dedicated) thread until `stop`.
fn run_output_stream(shared: Arc<ClientShared>, stop: Arc<AtomicBool>) {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        eprintln!("wavefront: no default output device");
        return;
    };
    let config = match device.default_output_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("wavefront: failed to get output config: {e}");
            return;
        }
    };
    let sample_format = config.sample_format();
    let mut stream_config: cpal::StreamConfig = config.into();
    // The stream carries 48 kHz PCM; force the device to that rate if it
    // supports it, otherwise playback would be pitch-shifted and drift.
    let supports_48k = device
        .supported_output_configs()
        .map(|mut cfgs| {
            cfgs.any(|c| {
                c.min_sample_rate().0 <= 48_000
                    && c.max_sample_rate().0 >= 48_000
                    && c.sample_format() == sample_format
            })
        })
        .unwrap_or(false);
    if supports_48k {
        stream_config.sample_rate = cpal::SampleRate(48_000);
    } else if stream_config.sample_rate.0 != 48_000 {
        eprintln!(
            "wavefront: output device does not support 48 kHz (using {} Hz) — audio may play at the wrong speed",
            stream_config.sample_rate.0
        );
    }
    let out_channels = stream_config.channels as usize;

    // Leftover already-due samples not yet written to the device.
    let carry: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));

    let err_fn = |err| eprintln!("wavefront: output stream error: {err}");

    macro_rules! run_with {
        ($sample_ty:ty, $convert:expr) => {{
            let shared = shared.clone();
            let carry = carry.clone();
            device.build_output_stream(
                &stream_config,
                move |data: &mut [$sample_ty], _: &cpal::OutputCallbackInfo| {
                    fill_output(&shared, &carry, data, out_channels, $convert);
                },
                err_fn,
                None,
            )
        }};
    }

    let stream = match sample_format {
        cpal::SampleFormat::F32 => run_with!(f32, |s: i16| s as f32 / i16::MAX as f32),
        cpal::SampleFormat::I16 => run_with!(i16, |s: i16| s),
        cpal::SampleFormat::U16 => run_with!(u16, |s: i16| (s as i32 + 32768) as u16),
        _ => {
            eprintln!("wavefront: unsupported output sample format");
            return;
        }
    };

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wavefront: failed to build output stream: {e}");
            return;
        }
    };
    if let Err(e) = stream.play() {
        eprintln!("wavefront: failed to start output stream: {e}");
        return;
    }

    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(stream);
}

fn fill_output<T: Copy>(
    shared: &Arc<ClientShared>,
    carry: &Arc<Mutex<VecDeque<i16>>>,
    data: &mut [T],
    out_channels: usize,
    convert: impl Fn(i16) -> T,
) {
    let now = wall_now_ms();

    // Pull any due chunks from the priority queue into the carry ring buffer,
    // applying DSP as they're dequeued.
    {
        let mut queue = shared.queue.lock();
        let mut dsp = shared.dsp.lock();
        while let Some(Reverse(top)) = queue.peek() {
            if top.play_at_local_ms <= now + 20.0 {
                let Reverse(mut chunk) = queue.pop().unwrap();
                if chunk.play_at_local_ms < now - 100.0 {
                    // Too late, drop.
                    continue;
                }
                dsp.process(&mut chunk.pcm);
                let mut c = carry.lock();
                c.extend(chunk.pcm.iter().copied());
            } else {
                break;
            }
        }
    }

    let frames = data.len() / out_channels;
    let mut c = carry.lock();
    for f in 0..frames {
        let (l, r) = match (c.pop_front(), c.pop_front()) {
            (Some(l), Some(r)) => (l, r),
            _ => (0, 0),
        };
        let base = f * out_channels;
        if out_channels == 1 {
            let m = ((l as i32 + r as i32) / 2) as i16;
            data[base] = convert(m);
        } else {
            data[base] = convert(l);
            data[base + 1] = convert(r);
            for ch in 2..out_channels {
                data[base + ch] = convert(0);
            }
        }
    }
}
