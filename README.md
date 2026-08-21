# Wavefront

Turn every laptop in the room into one synced speaker system.

One laptop is the **master**: it captures whatever audio the system is
playing (Spotify, a YouTube tab, a movie — anything) and streams it over the
LAN with sync timestamps. Every other laptop joins as a **speaker** — either
by opening a URL in a browser (zero install) or by running this same app in
client mode (tightest sync). The master assigns each speaker a role
(sub / tweeter / full-range), a channel (left / mid / right), a gain, and a
position in the room.

- No device limit — capacity depends only on your Wi-Fi; the master warns
  when the network gets weak.
- Cross-platform: macOS, Windows, Linux (Tauri 2 + Rust).
- Sync: NTP-style clock offset over the control WebSocket; audio chunks carry
  a master-clock play-at timestamp; clients schedule against it (~250 ms
  shared buffer).
- Crossover: 2nd-order Butterworth low/high-pass at an adjustable split
  point, applied on each client (Rust biquads natively, BiquadFilterNode in
  the browser).

## Layout

- `src-tauri/` — Rust backend: capture, stream server, native client, DSP.
- `ui/` — master dashboard + native client screen (Tauri frontend).
- `webclient/` — the zero-install browser speaker page, served by the master
  at `http://<master-ip>:8927/`.
- `PROTOCOL.md` — the wire protocol.

## System audio capture

- **Windows**: WASAPI loopback, works out of the box.
- **Linux**: PulseAudio/PipeWire monitor source, works out of the box.
- **macOS**: needs the free [BlackHole](https://existential.audio/blackhole/)
  virtual device (macOS has no public loopback API); without it Wavefront
  falls back to the microphone and shows a warning.

## Build

```
cd src-tauri
cargo tauri build     # or: cargo tauri dev
```
