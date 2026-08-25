# Wavefront

Turn every laptop and phone in the room into one synced speaker system — and
do it across networks that block device-to-device traffic (public/guest Wi-Fi),
with speakers that need **no install** (just open a URL in a browser).

One device is the **host**: it captures whatever audio is playing and streams
it out with sync timestamps. Every other device is a **speaker** — a browser
tab or the native app — and plays in sync, with a per-speaker role
(sub / tweeter / full-range), channel (left / mid / right), gain, and room
position assigned from the host.

## How it works

```
          ┌────────────┐   one stream    ┌──────────────┐   fan-out    ┌───────────┐
  audio → │   HOST     │ ───────────────▶│    RELAY     │ ────────────▶│ SPEAKERS  │
          │ Mac app or │  (/source, WS)  │  (public VPS)│  (/ws, WS)   │ browser / │
          │  browser   │                 │  fan-out +   │              │  native   │
          └────────────┘                 │  history     │              └───────────┘
                                         └──────────────┘
```

- **Relay fan-out.** The host uploads *one* stream to a small relay on a public
  VPS; the relay copies it to every speaker. Because every side only talks
  *outward* to the relay, LAN client-isolation never applies. No device limit —
  capacity scales with the network.
- **Sync.** Speakers estimate a shared clock with a 2-state **Kalman filter**
  (offset + drift) over the control WebSocket; each audio chunk carries a
  master-clock play-at timestamp; speakers schedule against it with a buffered,
  drift-corrected continuous cursor. Hardware **output latency** (incl.
  Bluetooth) is compensated so *acoustic* output aligns, not just the schedule.
- **Robustness.** Adjustable buffer (up to 10 s) rides out dropouts without
  stopping; new speakers are **backfilled** from the relay's rolling history so
  they start instantly *and* in sync; a speaker that can't trust its own sync
  (or that the group flags as a jitter outlier) **auto-mutes** rather than
  echo-ruining the room.
- **Bandwidth.** Audio is **Opus** by default (~0.06 Mbps/listener), with a
  24 kHz-PCM fallback.
- **Crossover.** 2nd-order Butterworth low/high-pass per speaker for sub/tweeter
  roles, applied client-side.

## Repo layout

| Path          | What                                                              |
|---------------|------------------------------------------------------------------|
| `src-tauri/`  | Native host+speaker app (Tauri 2 + Rust): capture, DSP, uplink   |
| `ui/`         | The app's dashboard / client UI (served as the Tauri frontend)   |
| `webclient/`  | Zero-install browser **speaker** (`index.html`) + browser **host** (`host.html`) |
| `relay/`      | Standalone Rust fan-out relay (deployed to the VPS)              |
| `relay/deploy/` | `deploy.sh` + systemd unit to stand the relay up on a VPS      |
| `PROTOCOL.md` | The wire protocol (frames, clock sync, relay extension)          |

## Build & run

**Native app (host or speaker):**
```
cd src-tauri
cargo tauri build      # or: cargo tauri dev
```
Needs the Tauri CLI (`cargo install tauri-cli --version ^2`) and, for Opus,
libopus (`brew install opus` on macOS; it links statically into the binary).

**Relay (on a VPS):**
```
relay/deploy/deploy.sh root@your.vps.ip
```
Then speakers open `http://<vps-ip>:8927/` and the app hosts via that relay.

## System-audio capture (host)

- **Windows:** WASAPI loopback — works out of the box.
- **Linux:** PulseAudio/PipeWire monitor source — works out of the box.
- **macOS:** needs the free [BlackHole](https://existential.audio/blackhole/)
  virtual device (macOS has no public loopback API); route output through a
  Multi-Output Device that includes BlackHole.
- **Browser host** (`host.html`): captures a tab/screen via `getDisplayMedia` —
  **Chromium only** (Chrome/Brave/Edge) and requires the page be served over
  **HTTPS**.

## Status / caveats

- Opus is the default codec; the browser Opus path relies on WebCodecs
  (Chromium solid; Safari/Firefox vary). PCM fallback exists.
- The browser host is Chromium + HTTPS only (see above).
- The relay keeps a small in-memory rolling history (~one buffer) for backfill;
  it is ephemeral (RAM, never disk) but note it briefly holds unencrypted audio.

Not affiliated with Spotify or any music service — it just relays whatever your
host device is already playing.
