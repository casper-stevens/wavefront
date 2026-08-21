//! System-audio capture via cpal, resampled/converted to 48kHz stereo s16le,
//! chunked into 20ms (960 frame) pieces and pushed onto a broadcast channel.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast;

pub const TARGET_SAMPLE_RATE: u32 = 48_000;
pub const CHUNK_FRAMES: usize = 960; // 20ms at 48kHz

#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Monotonic capture timestamp in milliseconds.
    pub capture_ts_ms: f64,
    /// Interleaved stereo s16le PCM, exactly CHUNK_FRAMES * 2 samples.
    pub pcm: Vec<i16>,
}

pub struct CaptureHandle {
    tx: broadcast::Sender<AudioChunk>,
    pub source_label: String,
    pub warnings: Vec<String>,
    stop: Arc<AtomicBool>,
}

impl CaptureHandle {
    pub fn subscribe(&self) -> broadcast::Receiver<AudioChunk> {
        self.tx.subscribe()
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Picks the capture device per OS loopback strategy, returning the device,
/// a human label, and any warnings.
fn pick_device(host: &cpal::Host) -> (cpal::Device, String, Vec<String>) {
    let mut warnings = Vec::new();

    #[cfg(target_os = "windows")]
    {
        if let Some(dev) = host.default_output_device() {
            let name = dev.name().unwrap_or_else(|_| "default output".into());
            return (dev, format!("WASAPI loopback: {name}"), warnings);
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(devices) = host.input_devices() {
            for dev in devices {
                if let Ok(name) = dev.name() {
                    if name.contains("BlackHole") {
                        return (dev, format!("BlackHole: {name}"), warnings);
                    }
                }
            }
        }
        warnings.push(
            "Install BlackHole (existential.audio/blackhole) and set it as a Multi-Output to capture system audio — currently using the microphone".to_string(),
        );
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(devices) = host.input_devices() {
            for dev in devices {
                if let Ok(name) = dev.name() {
                    if name.contains(".monitor") {
                        return (dev, format!("Monitor source: {name}"), warnings);
                    }
                }
            }
        }
        warnings.push(
            "Using microphone — select a monitor source in pavucontrol".to_string(),
        );
    }

    let dev = host
        .default_input_device()
        .expect("no default input device available");
    let name = dev.name().unwrap_or_else(|_| "default input".into());
    (dev, format!("Microphone: {name}"), warnings)
}

/// Naive linear resampler + channel mixer, converting arbitrary input format
/// to 48kHz interleaved stereo f32.
fn convert_to_stereo_48k(
    input: &[f32],
    in_channels: usize,
    in_rate: u32,
    out: &mut Vec<f32>,
) {
    if in_channels == 0 {
        return;
    }
    let in_frames = input.len() / in_channels;

    // Downmix/upmix to stereo first.
    let mut stereo: Vec<[f32; 2]> = Vec::with_capacity(in_frames);
    for f in 0..in_frames {
        let base = f * in_channels;
        let (l, r) = if in_channels == 1 {
            let m = input[base];
            (m, m)
        } else {
            (input[base], input[base + 1])
        };
        stereo.push([l, r]);
    }

    if in_rate == TARGET_SAMPLE_RATE || stereo.is_empty() {
        for [l, r] in stereo {
            out.push(l);
            out.push(r);
        }
        return;
    }

    // Naive linear resampling.
    let ratio = in_rate as f64 / TARGET_SAMPLE_RATE as f64;
    let out_frames = ((stereo.len() as f64) / ratio).floor() as usize;
    for i in 0..out_frames {
        let src_pos = i as f64 * ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let idx_next = (idx + 1).min(stereo.len() - 1);
        let l = stereo[idx][0] * (1.0 - frac) + stereo[idx_next][0] * frac;
        let r = stereo[idx][1] * (1.0 - frac) + stereo[idx_next][1] * frac;
        out.push(l);
        out.push(r);
    }
}

/// Spawns the capture thread and returns a handle with a broadcast receiver.
pub fn start_capture() -> anyhow::Result<CaptureHandle> {
    let host = cpal::default_host();
    let (device, source_label, warnings) = pick_device(&host);

    let config = device.default_input_config()?;
    let sample_format = config.sample_format();
    let stream_config: StreamConfig = config.clone().into();
    let in_channels = stream_config.channels as usize;
    let in_rate = stream_config.sample_rate.0;

    let (tx, _rx) = broadcast::channel::<AudioChunk>(64);
    let tx_for_handle = tx.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    let start = Instant::now();

    std::thread::spawn(move || {
        // Leftover f32 stereo samples not yet emitted as a full 20ms chunk.
        let mut pending: Vec<f32> = Vec::new();
        let mut scratch: Vec<f32> = Vec::new();

        let err_fn = |err| eprintln!("wavefront: capture stream error: {err}");

        let result: Result<cpal::Stream, cpal::BuildStreamError> = match sample_format {
            SampleFormat::F32 => device.build_input_stream(
                &stream_config,
                {
                    let tx = tx.clone();
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        handle_input(data, in_channels, in_rate, &start, &tx, &mut pending, &mut scratch);
                    }
                },
                err_fn,
                None,
            ),
            SampleFormat::I16 => {
                let mut pending2: Vec<f32> = Vec::new();
                let mut scratch2: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &stream_config,
                    {
                        let tx = tx.clone();
                        move |data: &[i16], _: &cpal::InputCallbackInfo| {
                            let f32_data: Vec<f32> =
                                data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                            handle_input(
                                &f32_data, in_channels, in_rate, &start, &tx, &mut pending2, &mut scratch2,
                            );
                        }
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut pending3: Vec<f32> = Vec::new();
                let mut scratch3: Vec<f32> = Vec::new();
                device.build_input_stream(
                    &stream_config,
                    {
                        let tx = tx.clone();
                        move |data: &[u16], _: &cpal::InputCallbackInfo| {
                            let f32_data: Vec<f32> = data
                                .iter()
                                .map(|s| (*s as f32 - 32768.0) / 32768.0)
                                .collect();
                            handle_input(
                                &f32_data, in_channels, in_rate, &start, &tx, &mut pending3, &mut scratch3,
                            );
                        }
                    },
                    err_fn,
                    None,
                )
            }
            other => {
                eprintln!("wavefront: unsupported sample format {other:?}");
                return;
            }
        };

        let stream = match result {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wavefront: failed to build capture stream: {e}");
                return;
            }
        };

        if let Err(e) = stream.play() {
            eprintln!("wavefront: failed to start capture stream: {e}");
            return;
        }

        while !stop_thread.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        drop(stream);
    });

    Ok(CaptureHandle {
        tx: tx_for_handle,
        source_label,
        warnings,
        stop,
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_input(
    data: &[f32],
    in_channels: usize,
    in_rate: u32,
    _start: &Instant,
    tx: &broadcast::Sender<AudioChunk>,
    pending: &mut Vec<f32>,
    scratch: &mut Vec<f32>,
) {
    scratch.clear();
    convert_to_stereo_48k(data, in_channels, in_rate, scratch);
    pending.extend_from_slice(scratch);

    let samples_per_chunk = CHUNK_FRAMES * 2;
    while pending.len() >= samples_per_chunk {
        let chunk_f32: Vec<f32> = pending.drain(0..samples_per_chunk).collect();
        let pcm: Vec<i16> = chunk_f32
            .iter()
            .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();
        // MUST be the same clock the server's pong `t1` uses (wall epoch ms):
        // clients compute play offsets against that clock, and mixing clocks
        // makes every chunk look infinitely late.
        let capture_ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0;
        let _ = tx.send(AudioChunk { capture_ts_ms, pcm });
    }
}
