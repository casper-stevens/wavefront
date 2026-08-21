# Wavefront Protocol v1

One master, N speaker clients (native app in client mode, or browser tab).
All communication over a single WebSocket per client: `ws://<master>:8927/ws`.
The master also serves the browser client at `http://<master>:8927/`.

## Audio format

- 48000 Hz, stereo, interleaved **s16le** PCM.
- Chunk duration: 20 ms (960 frames, 1920 samples, 3840 bytes payload).

## Clock sync

Client-driven NTP-style over the same WebSocket, JSON text frames:

- Client → `{"type":"ping","t0":<client_ms_f64>}`
- Master → `{"type":"pong","t0":<echoed>,"t1":<master_ms_f64>}`

`offset = t1 + rtt/2 - t_recv` where `rtt = t_recv - t0`. Clients keep the
median of the last 15 samples, pinging every 2 s. All times are milliseconds
as f64 (`performance.now()`-style monotonic on each side; the master uses its
own monotonic ms clock for both pongs and chunk timestamps).

## Binary audio frames (master → client)

Little-endian header, then payload:

| offset | type | field |
|---|---|---|
| 0 | u8  | frame kind = 0x01 (audio) |
| 1 | u8  | flags (reserved, 0) |
| 2 | u16 | reserved |
| 4 | f64 | `play_at` — master-clock ms when this chunk must start playing |
| 12 | ...  | s16le interleaved stereo PCM |

`play_at = capture_time + buffer_ms` (master adds the shared buffer, default
250 ms). A client converts: `local_play_at = play_at - offset`, schedules the
chunk, and drops chunks whose time has already passed.

## Control messages (JSON text frames)

Client → master:
- `{"type":"hello","name":<string>,"kind":"native"|"browser"}` — first message.
- `{"type":"ping",...}` — see clock sync.

Master → client:
- `{"type":"welcome","id":<u32>,"sample_rate":48000,"buffer_ms":<u32>}`
- `{"type":"config","role":"sub"|"tweeter"|"full","pan":"left"|"mid"|"right","gain":<0..1 f32>,"crossover_hz":<f32>,"buffer_ms":<u32>}`
  — sent on join and whenever the master UI changes anything for this client.
- `{"type":"pong",...}` — see clock sync.

Client applies its config locally: biquad low-pass (sub) or high-pass
(tweeter) at `crossover_hz` (2nd-order Butterworth, Q=0.7071), gain, and
channel selection (left = L only to both ears, right = R only, mid = L+R mix).

## Master UI ↔ backend (Tauri commands / events)

Commands (invoke): `start_master`, `stop_master`, `set_client_config{id,role,pan,gain}`,
`set_crossover{hz}`, `set_master_volume{v}`, `set_master_plays{on}`,
`start_client{addr}`, `stop_client`, `get_status`.

Event (backend → UI): `wavefront://state` with the full serialized state
(mode, clients list with id/name/kind/latency/config, capture source label,
warnings) — emitted on every change and at 1 Hz.
