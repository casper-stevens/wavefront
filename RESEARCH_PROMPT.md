# Research prompt — improving browser-based multi-device audio sync

## Context: what exists

I have a working system ("Wavefront") that plays the same audio on many
laptops and phones at once, turning them into a synchronized speaker array.
Architecture:

- A **master** (native app) captures system audio and uploads ONE stream to a
  **relay** server on a public VPS.
- The relay fans that stream out to **N clients**. Clients are either a native
  app or, importantly, **a plain web page opened in any browser** (no install).
- Audio wire format: 24 kHz, 16-bit, stereo PCM, in 20 ms chunks, over a
  WebSocket. The relay stamps each chunk with a `play_at` time in the relay's
  clock (derived from the master's capture timeline via a stable offset).
- Each browser client does **NTP-style clock sync** over the same WebSocket:
  it pings, the relay replies with its clock, the client computes
  `offset = t1 + rtt/2 - tRecv`, keeps a smoothed estimate from low-RTT
  samples, and schedules playback so every device targets the same absolute
  moment. Playback uses a continuous cursor with a ~1 s buffer, drift-corrected
  by small (±0.3%) `playbackRate` nudges, built from `AudioBufferSourceNode`s.

## The constraints (these are FIXED — solutions must respect them)

1. **Clients must run in an unmodified browser** — no install, no native code,
   no browser flags. Must include **iOS (WebKit)** — Safari, and Chrome/Brave/
   Firefox on iOS all use WebKit.
2. **Clients may be on different networks** reached via an internet relay
   (public Wi-Fi with client isolation, cellular, etc.), so LAN-only tech
   (PTP/IEEE-1588, mDNS, multicast, AES67/Dante/AVB) is out.
3. Goal: get inter-device playback agreement as tight as possible — ideally
   inside the ~10–30 ms psychoacoustic fusion (Haas) window — and be robust to
   jitter, background-tab throttling, and iOS audio-session interruptions.

Current real-world result: RTTs of ~50–150 ms per device over the relay;
inter-device agreement in the tens of ms, occasionally audible when two
speakers are side by side.

## What I want researched (be specific, cite browser + iOS/WebKit support)

For each item: does it exist, is it supported in current browsers **including
iOS WebKit**, what accuracy/latency improvement is realistic, and a concrete
code pattern or gotcha.

1. **Clock sync precision in-browser.** Is there anything better than
   `performance.now()` + a WebSocket round-trip for estimating a shared clock?
   - Does **cross-origin isolation** (COOP/COEP) meaningfully raise
     `performance.now()` resolution, and does it matter here?
   - Can **WebRTC** help? Its RTP/RTCP sender reports carry NTP timestamps, and
     `RTCRtpReceiver.getSynchronizationSources()` / `getStats()` expose timing.
     Could a WebRTC DataChannel or media track give a better shared clock or
     lower-jitter transport than WebSocket? Does any of this work on iOS?
   - **WebTransport** (HTTP/3 / QUIC) — does it help transport jitter or timing
     vs WebSocket, and what's the iOS support story?

2. **Sample-accurate output timing.**
   - `AudioContext.getOutputTimestamp()` returns `{contextTime,
     performanceTime}` — can I use it to precisely relate the audio hardware
     clock to `performance.now()`, improving my context↔perf mapping and
     measuring/compensating DAC drift? Support incl. iOS?
   - Is an **AudioWorklet** pulling from a SharedArrayBuffer ring buffer
     materially better than scheduling many `AudioBufferSourceNode`s for
     glitch-free, precisely-timed continuous playback? iOS AudioWorklet caveats?
   - Can I measure the actual output sample rate / DAC drift per device and
     correct it (resampling or rate) more precisely than a feedback loop?

3. **Distributed-timing standards for browsers.** Investigate the **W3C
   Multi-Device Timing Community Group / Timing Object** spec and the
   `timingsrc` library (Ingar Arntzen), plus any HbbTV/DVB **companion-screen
   synchronization** (CSS-WC / DVB-CSS wall clock) work. Are these usable,
   maintained, and do they beat a hand-rolled NTP loop? Do they assume LAN?

4. **Compression.** **WebCodecs** `AudioEncoder`/`AudioDecoder` with **Opus** —
   current support incl. iOS WebKit, latency of encode/decode, and whether it's
   viable to cut the ~0.77 Mbps/listener PCM stream to ~0.1 Mbps without hurting
   sync. Fallbacks if WebCodecs is missing.

5. **iOS/WebKit robustness.**
   - The **Media Session API** and any audio-session category control — can a
     web page reliably play through the iOS silent switch and survive
     interruptions without the silent-`<audio>` hack?
   - Background-tab / lock-screen throttling: what actually keeps audio
     scheduling alive on iOS when the screen locks or the tab backgrounds?
     Wake Lock support and limits.
   - Why might a WebSocket to a raw `IP:port` fail specifically in desktop
     Safari (iCloud Private Relay? HTTPS upgrade? mixed content?) and how to
     detect/work around it from the page.

6. **Anything else** that materially improves multi-device browser audio sync
   over the internet that I haven't listed. Real projects doing this well
   (Snapcast is the LAN reference; who does it over the internet in-browser?).

## Output format I want

A prioritized list: for each promising technique — the API, browser + iOS
support as of now, realistic accuracy/latency/bandwidth impact, integration
effort, and a short code sketch or the key gotcha. Flag clearly anything that
does NOT work on iOS WebKit, since that's my hardest target.
