"use strict";

/*
 * Wavefront browser speaker client.
 * Implements PROTOCOL.md: WebSocket control + binary audio frames,
 * NTP-style clock sync, and a live-rewireable DSP graph
 * (biquad crossover -> gain -> pan) per config messages from the master.
 */

(function () {
  const SAMPLE_RATE = 24000; // wire stream rate (half-rate to save bandwidth)
  const FRAMES_PER_CHUNK = 480; // 20ms at 24kHz
  const HEADER_BYTES = 12;
  const PAYLOAD_BYTES = FRAMES_PER_CHUNK * 2 * 2; // stereo s16le
  const FRAME_BYTES = HEADER_BYTES + PAYLOAD_BYTES;
  const Q_BUTTERWORTH = 0.70710678;
  const SYNC_SAMPLES = 15;
  const PING_INTERVAL_MS = 2000;
  const RECONNECT_MIN_MS = 1000;
  const RECONNECT_MAX_MS = 10000;

  // ---- DOM ----
  const joinScreen = document.getElementById("joinScreen");
  const app = document.getElementById("app");
  const joinBtn = document.getElementById("joinBtn");
  const nameInput = document.getElementById("nameInput");
  const statusPill = document.getElementById("statusPill");
  const roleVal = document.getElementById("roleVal");
  const panVal = document.getElementById("panVal");
  const gainVal = document.getElementById("gainVal");
  const crossoverVal = document.getElementById("crossoverVal");
  const bufferVal = document.getElementById("bufferVal");
  const offsetVal = document.getElementById("offsetVal");
  const rttVal = document.getElementById("rttVal");
  const scheduledVal = document.getElementById("scheduledVal");
  const droppedVal = document.getElementById("droppedVal");
  const glitchVal = document.getElementById("glitchVal");
  const muteBtn = document.getElementById("muteBtn");
  const hiddenWarning = document.getElementById("hiddenWarning");
  const hostLabel = document.getElementById("hostLabel");

  hostLabel.textContent = location.host;
  nameInput.value = "Browser on " + (navigator.platform || "device");

  // ---- state ----
  let ws = null;
  let joined = false;
  let reconnectDelay = RECONNECT_MIN_MS;
  let reconnectTimer = null;
  let clientName = "";

  let audioCtx = null;
  let joinPerfMs = 0;   // performance.now() sampled at join
  let joinCtxTime = 0;  // audioContext.currentTime sampled at join

  let currentOffset = 0;
  let currentRtt = 0;
  let minRtt = null;       // rolling minimum RTT baseline (ms)
  let offsetInit = false;  // has the offset been seeded yet?
  let pingTimer = null;

  let scheduledCount = 0;
  let droppedCount = 0;
  let glitchCount = 0; // audio-session interruptions (iOS pauses the context)

  // Continuous playback cursor (audioContext time of the next chunk to play).
  // Chunks are scheduled back-to-back from here rather than each at its own
  // absolute timestamp, so network-jitter bursts and clock-offset updates
  // don't create audible seams. Reset to 0 to force a re-anchor.
  let playHead = 0;
  let badSince = 0; // audioCtx time the cursor first went off-target (watchdog)
  const CHUNK_DUR = FRAMES_PER_CHUNK / SAMPLE_RATE; // 0.02s

  // Smoothed offset between the audio clock and performance.now() (seconds):
  // ctxTime ≈ perfMs/1000 + ctxPerfK. Reading audioContext.currentTime raw each
  // chunk is noisy on platforms where it advances in coarse steps (Linux /
  // Firefox), which jitters the play target; low-passing it keeps the target
  // stable. null until the first chunk anchors it; reset with playHead.
  let ctxPerfK = null;

  let muted = false;
  let wakeLock = null;

  let currentConfig = { role: "full", pan: "mid", gain: 1, crossover_hz: 2000, buffer_ms: 250 };

  // ---- DSP graph nodes (built once, rewired on config change) ----
  let filterNode = null;      // biquad, used for sub/tweeter
  let passthroughNode = null; // unity gain, used for "full"
  let gainNode = null;        // config.gain
  let splitter = null;        // ChannelSplitterNode(2)
  let merger = null;          // ChannelMergerNode(2)
  let midGainL = null;
  let midGainR = null;
  let muteGain = null;        // master mute control

  function setStatus(text, cls) {
    statusPill.textContent = text;
    statusPill.className = "pill " + cls;
  }

  function median(arr) {
    if (arr.length === 0) return 0;
    const sorted = arr.slice().sort((a, b) => a - b);
    const mid = Math.floor(sorted.length / 2);
    if (sorted.length % 2 === 0) return (sorted[mid - 1] + sorted[mid]) / 2;
    return sorted[mid];
  }

  // ---------------------------------------------------------------------
  // Audio graph setup
  // ---------------------------------------------------------------------

  function buildAudioGraph() {
    filterNode = audioCtx.createBiquadFilter();
    filterNode.type = "lowpass";
    filterNode.Q.value = Q_BUTTERWORTH;

    passthroughNode = audioCtx.createGain();
    passthroughNode.gain.value = 1;

    gainNode = audioCtx.createGain();
    gainNode.gain.value = 1;

    splitter = audioCtx.createChannelSplitter(2);
    merger = audioCtx.createChannelMerger(2);

    midGainL = audioCtx.createGain();
    midGainL.gain.value = 0.5;
    midGainR = audioCtx.createGain();
    midGainR.gain.value = 0.5;

    muteGain = audioCtx.createGain();
    muteGain.gain.value = 1;

    filterNode.connect(gainNode);
    passthroughNode.connect(gainNode);
    gainNode.connect(splitter);
    merger.connect(muteGain);
    muteGain.connect(audioCtx.destination);

    applyConfigToGraph(currentConfig);
  }

  function safeDisconnect(node) {
    try { node.disconnect(); } catch (e) { /* not connected, ignore */ }
  }

  function rewirePan(pan) {
    safeDisconnect(splitter);
    safeDisconnect(midGainL);
    safeDisconnect(midGainR);

    if (pan === "left") {
      splitter.connect(merger, 0, 0);
      splitter.connect(merger, 0, 1);
    } else if (pan === "right") {
      splitter.connect(merger, 1, 0);
      splitter.connect(merger, 1, 1);
    } else {
      // mid: (L + R) / 2 to both output channels
      splitter.connect(midGainL, 0);
      splitter.connect(midGainR, 1);
      midGainL.connect(merger, 0, 0);
      midGainL.connect(merger, 0, 1);
      midGainR.connect(merger, 0, 0);
      midGainR.connect(merger, 0, 1);
    }
  }

  function applyConfigToGraph(cfg) {
    if (!audioCtx) return;

    if (cfg.role === "sub") {
      filterNode.type = "lowpass";
    } else if (cfg.role === "tweeter") {
      filterNode.type = "highpass";
    }
    if (typeof cfg.crossover_hz === "number") {
      filterNode.frequency.setValueAtTime(cfg.crossover_hz, audioCtx.currentTime);
    }
    filterNode.Q.setValueAtTime(Q_BUTTERWORTH, audioCtx.currentTime);

    if (typeof cfg.gain === "number") {
      gainNode.gain.setValueAtTime(cfg.gain, audioCtx.currentTime);
    }

    rewirePan(cfg.pan || "mid");
  }

  function connectSourceForRole(source, role) {
    if (role === "full") {
      source.connect(passthroughNode);
    } else {
      source.connect(filterNode);
    }
  }

  // ---------------------------------------------------------------------
  // Clock sync
  // ---------------------------------------------------------------------

  function sendPing() {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    ws.send(JSON.stringify({ type: "ping", t0: performance.now() }));
  }

  function handlePong(msg) {
    const tRecv = performance.now();
    const t0 = msg.t0;
    const t1 = msg.t1;
    const rtt = tRecv - t0;
    const offset = t1 + rtt / 2 - tRecv;

    // Rolling minimum-RTT baseline: snap down to any new low, leak up slowly so
    // it adapts to changing network conditions instead of sticking forever to
    // one lucky ping.
    if (minRtt === null || rtt < minRtt) minRtt = rtt;
    else minRtt += Math.min(rtt - minRtt, 0.5);

    // Only trust the offset from low-queue pings (RTT near the baseline) — a
    // congested ping's offset is skewed. Fold accepted samples in with a gentle
    // EMA so the offset is SMOOTH and continuously self-healing. The old
    // min-over-a-sliding-window approach stepped whenever the best sample aged
    // out, nudging each device differently — that's what drifted them apart and
    // only a reload fixed. A smooth estimate holds them together and recovers
    // on its own.
    if (rtt <= minRtt + 8) {
      if (!offsetInit) {
        currentOffset = offset;
        offsetInit = true;
      } else {
        currentOffset += (offset - currentOffset) * 0.15;
      }
    }
    currentRtt = currentRtt === 0 ? rtt : currentRtt + (rtt - currentRtt) * 0.2;

    offsetVal.textContent = currentOffset.toFixed(1) + " ms";
    rttVal.textContent = currentRtt.toFixed(1) + " ms";
    if (offsetInit) setStatus("synced", "synced");
  }

  // ---------------------------------------------------------------------
  // Binary audio frame handling
  // ---------------------------------------------------------------------

  function handleAudioFrame(buffer) {
    if (buffer.byteLength < FRAME_BYTES) return;
    const view = new DataView(buffer);

    const kind = view.getUint8(0);
    if (kind !== 0x01) return;

    // Convert s16le interleaved stereo PCM -> Float32 planar
    const samples = new Int16Array(buffer, HEADER_BYTES, FRAMES_PER_CHUNK * 2);
    const audioBuffer = audioCtx.createBuffer(2, FRAMES_PER_CHUNK, SAMPLE_RATE);
    const chL = audioBuffer.getChannelData(0);
    const chR = audioBuffer.getChannelData(1);
    for (let i = 0; i < FRAMES_PER_CHUNK; i++) {
      chL[i] = samples[i * 2] / 32768;
      chR[i] = samples[i * 2 + 1] / 32768;
    }

    const playAt = view.getFloat64(4, true);

    // Phase-locked continuous cursor. TCP keeps chunks in order, so we play
    // them back-to-back from a moving playHead for gapless audio — but we
    // steer that cursor toward the SHARED relay clock so every device converges
    // on the same wall-clock playback time and can't drift apart.
    //
    // `targetCtx` is when THIS chunk should play, in local audio-clock seconds,
    // derived from the relay's timestamp (already includes the buffer lead) and
    // this device's own clock offset. Same on every device up to offset error,
    // and recomputed live each chunk so it also absorbs perf/audio-clock skew.
    const now = audioCtx.currentTime;
    const nowMs = performance.now();
    // Low-pass the audio-vs-perf clock offset so a coarse currentTime doesn't
    // jitter the target (this was the Linux/Firefox stutter).
    const kInstant = now - nowMs / 1000;
    if (ctxPerfK === null) ctxPerfK = kInstant;
    else ctxPerfK += (kInstant - ctxPerfK) * 0.05;
    const localPlayAtMs = playAt - currentOffset;
    const targetCtx = localPlayAtMs / 1000 + ctxPerfK;

    // Correct drift by nudging PLAYBACK RATE, not chunk start times. Buffers
    // stay perfectly contiguous (gapless — no seams on any platform), while a
    // sub-percent rate change smoothly steers the cursor onto the shared clock.
    // Only a genuine underrun or large loss-of-lock triggers a hard re-anchor.
    // Watchdog: if the cursor sits more than ~35ms off target for a sustained
    // stretch, the ±0.3% rate-lock can't close it (saturated drift, a wrong
    // offset lock, or a stuck state after an interruption). Auto-resync — hard
    // re-anchor AND drop the clock offset so it re-estimates fresh — which is
    // exactly what a manual reload did, now automatic.
    const err0 = playHead - targetCtx;
    if (playHead !== 0 && Math.abs(err0) > 0.035) {
      if (badSince === 0) badSince = now;
    } else {
      badSince = 0;
    }
    const watchdogTrip = badSince !== 0 && now - badSince > 2.0;

    let rate = 1;
    if (
      playHead === 0 ||
      playHead < now + 0.005 ||               // underrun / first chunk
      Math.abs(playHead - targetCtx) > 0.2 ||  // lost lock: hard re-anchor
      watchdogTrip                             // sustained off-target: auto-resync
    ) {
      if (playHead !== 0 && playHead < now + 0.005) {
        droppedCount++;
        droppedVal.textContent = String(droppedCount);
      }
      if (watchdogTrip) {
        offsetInit = false; // re-estimate the clock offset from fresh pings
        minRtt = null;
      }
      badSince = 0;
      playHead = Math.max(targetCtx, now + 0.005);
    } else {
      // error > 0 → cursor ahead of target (buffer too deep) → play a hair
      // faster to drain; error < 0 → play a hair slower. Clamp to ±0.3%
      // (~5 cents, inaudible). Drives inter-device error to zero smoothly.
      const error = playHead - targetCtx;
      rate = Math.max(0.997, Math.min(1.003, 1 + 0.5 * error));
    }

    const startAt = playHead;
    playHead += CHUNK_DUR / rate; // buffer occupies CHUNK_DUR/rate of real time

    const source = audioCtx.createBufferSource();
    source.buffer = audioBuffer;
    if (rate !== 1) source.playbackRate.value = rate;
    connectSourceForRole(source, currentConfig.role);

    scheduledCount++;
    scheduledVal.textContent = String(scheduledCount);
    source.onended = function () {
      scheduledCount = Math.max(0, scheduledCount - 1);
      scheduledVal.textContent = String(scheduledCount);
    };

    try {
      source.start(startAt);
    } catch (e) {
      scheduledCount = Math.max(0, scheduledCount - 1);
      scheduledVal.textContent = String(scheduledCount);
    }
  }

  // ---------------------------------------------------------------------
  // WebSocket / control messages
  // ---------------------------------------------------------------------

  function wsUrl() {
    const proto = location.protocol === "https:" ? "wss://" : "ws://";
    return proto + location.host + "/ws";
  }

  function connect() {
    setStatus("connecting", "connecting");
    ws = new WebSocket(wsUrl());
    ws.binaryType = "arraybuffer";

    ws.onopen = function () {
      reconnectDelay = RECONNECT_MIN_MS;
      ws.send(JSON.stringify({ type: "hello", name: clientName, kind: "browser" }));
      sendPing();
      pingTimer = setInterval(sendPing, PING_INTERVAL_MS);
    };

    ws.onmessage = function (evt) {
      if (evt.data instanceof ArrayBuffer) {
        handleAudioFrame(evt.data);
        return;
      }
      let msg;
      try {
        msg = JSON.parse(evt.data);
      } catch (e) {
        return;
      }
      switch (msg.type) {
        case "welcome":
          currentConfig.buffer_ms = msg.buffer_ms;
          bufferVal.textContent = msg.buffer_ms + " ms";
          break;
        case "config":
          currentConfig = Object.assign({}, currentConfig, msg);
          applyConfigToGraph(currentConfig);
          renderConfig();
          break;
        case "pong":
          handlePong(msg);
          break;
        default:
          break;
      }
    };

    ws.onclose = function () {
      setStatus("dropped", "dropped");
      clearInterval(pingTimer);
      minRtt = null;
      offsetInit = false;
      playHead = 0; // re-anchor the cursor on reconnect
      ctxPerfK = null;
      if (joined) scheduleReconnect();
    };

    ws.onerror = function () {
      try { ws.close(); } catch (e) { /* ignore */ }
    };
  }

  function scheduleReconnect() {
    if (reconnectTimer) return;
    reconnectTimer = setTimeout(function () {
      reconnectTimer = null;
      connect();
    }, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, RECONNECT_MAX_MS);
  }

  function renderConfig() {
    roleVal.textContent = currentConfig.role || "-";
    panVal.textContent = currentConfig.pan || "-";
    gainVal.textContent = (typeof currentConfig.gain === "number") ? currentConfig.gain.toFixed(2) : "-";
    crossoverVal.textContent = (typeof currentConfig.crossover_hz === "number") ? currentConfig.crossover_hz + " Hz" : "-";
    bufferVal.textContent = (typeof currentConfig.buffer_ms === "number") ? currentConfig.buffer_ms + " ms" : "-";
  }

  // ---------------------------------------------------------------------
  // Mute
  // ---------------------------------------------------------------------

  muteBtn.addEventListener("click", function () {
    muted = !muted;
    if (muteGain && audioCtx) {
      muteGain.gain.setValueAtTime(muted ? 0 : 1, audioCtx.currentTime);
    }
    muteBtn.textContent = muted ? "Unmute" : "Mute";
    muteBtn.classList.toggle("muted", muted);
  });

  // ---------------------------------------------------------------------
  // Wake lock
  // ---------------------------------------------------------------------

  async function requestWakeLock() {
    try {
      if ("wakeLock" in navigator) {
        wakeLock = await navigator.wakeLock.request("screen");
      }
    } catch (e) {
      // best-effort only
    }
  }

  document.addEventListener("visibilitychange", function () {
    hiddenWarning.classList.toggle("show", document.hidden);
    if (!document.hidden && joined) {
      requestWakeLock();
    }
  });

  // Build a valid silent WAV data URL (8kHz mono, `secs` of zero samples).
  // A real, non-empty payload is important — a zero-length WAV can make iOS
  // never fire load/play events.
  function makeSilentWavUrl(secs) {
    const rate = 8000;
    const n = Math.floor(rate * secs);
    const dataLen = n * 2; // 16-bit mono
    const buf = new ArrayBuffer(44 + dataLen);
    const dv = new DataView(buf);
    const w = function (off, s) { for (let i = 0; i < s.length; i++) dv.setUint8(off + i, s.charCodeAt(i)); };
    w(0, "RIFF"); dv.setUint32(4, 36 + dataLen, true); w(8, "WAVE");
    w(12, "fmt "); dv.setUint32(16, 16, true); dv.setUint16(20, 1, true);
    dv.setUint16(22, 1, true); dv.setUint32(24, rate, true);
    dv.setUint32(28, rate * 2, true); dv.setUint16(32, 2, true); dv.setUint16(34, 16, true);
    w(36, "data"); dv.setUint32(40, dataLen, true);
    // samples already zero
    let bin = "";
    const bytes = new Uint8Array(buf);
    for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
    return "data:audio/wav;base64," + btoa(bin);
  }

  // ---------------------------------------------------------------------
  // Join
  // ---------------------------------------------------------------------

  joinBtn.addEventListener("click", async function () {
    if (joined) return;
    clientName = (nameInput.value || "").trim() || ("Browser on " + (navigator.platform || "device"));
    joined = true;
    joinBtn.disabled = true;

    // Do NOT force the context to the 24kHz stream rate — some platforms
    // (older iOS/WebKit) reject a non-hardware AudioContext rate. Run the
    // context at the hardware rate and let Web Audio resample our 24kHz
    // AudioBuffers on playback (createBuffer takes their own sample rate).
    audioCtx = new (window.AudioContext || window.webkitAudioContext)();
    if (audioCtx.state === "suspended") {
      try { await audioCtx.resume(); } catch (e) { /* ignore */ }
    }

    // iOS/Safari/Firefox pause the AudioContext on audio-session interruptions
    // (a notification chime, a brief route change, backgrounding). That causes a
    // short silence that never touches the frame path — so it wouldn't show in
    // the counters. Detect it: count it, flash the status, force a resume, and
    // reset the cursor so playback re-anchors cleanly instead of trying to catch
    // up on stale scheduled buffers.
    audioCtx.onstatechange = function () {
      if (!audioCtx) return;
      const st = audioCtx.state;
      if (st === "interrupted" || st === "suspended") {
        glitchCount++;
        glitchVal.textContent = String(glitchCount);
        setStatus("interrupted", "connecting");
        playHead = 0;
        ctxPerfK = null;
        minRtt = null;
        offsetInit = false; // re-estimate the clock after the gap
        audioCtx.resume().catch(function () {});
      } else if (st === "running") {
        if (joined && ws && ws.readyState === WebSocket.OPEN) setStatus("synced", "synced");
      }
    };

    // iOS silent-switch workaround: by default an AudioContext plays in the
    // "ambient" session and is muted by the physical ring/silent switch.
    // Playing an <audio> element on this same user gesture promotes WebKit's
    // audio session to "playback", which ignores the switch. Fire-and-forget —
    // must NOT be awaited: on iOS a media element can leave play()'s promise
    // pending indefinitely, which previously stalled the whole join.
    try {
      const el = document.createElement("audio");
      el.loop = true;
      el.setAttribute("playsinline", "");
      // 1s of real (silent) 8kHz mono PCM so the element actually loads/plays.
      el.src = makeSilentWavUrl(1.0);
      el.volume = 0.01;
      const p = el.play();
      if (p && p.catch) p.catch(function () {});
      window.__wf_silence = el; // keep a reference so it isn't GC'd
    } catch (e) { /* ignore */ }

    joinPerfMs = performance.now();
    joinCtxTime = audioCtx.currentTime;

    buildAudioGraph();
    requestWakeLock();

    joinScreen.style.display = "none";
    app.classList.add("show");

    connect();
  });
})();
