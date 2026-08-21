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
  const PING_INTERVAL_MS = 500; // frequent pings sharpen the min-RTT offset estimate
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
  const outLatVal = document.getElementById("outLatVal");
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

  let currentOffset = 0;   // latest offset estimate (ms) — filter's theta
  let currentRtt = 0;
  let minRtt = null;       // rolling minimum RTT baseline (ms)
  let offsetInit = false;  // has the filter been seeded yet?
  let pingTimer = null;

  // 2-state Kalman filter for clock sync. State x = [theta, gamma]:
  //   theta = clock offset (relay_clock - local performance clock), ms
  //   gamma = drift rate of that offset, dimensionless (ms per ms)
  // Modelling the drift (gamma) as a state lets the filter EXTRAPOLATE the
  // offset smoothly between pings and discount asymmetric-jitter measurements,
  // which linear smoothing can't. P is the 2x2 error covariance.
  let kfTheta = 0;
  let kfGamma = 0;
  let kfP00 = 1e6, kfP01 = 0, kfP10 = 0, kfP11 = 1;
  let kfLastMs = 0; // performance.now() of the last filter update
  const KF_Q = 1e-9; // process-noise density (oscillator volatility), tunable

  let scheduledCount = 0;
  let droppedCount = 0;
  let glitchCount = 0; // audio-session interruptions (iOS pauses the context)

  // Continuous playback cursor (audioContext time of the next chunk to play).
  // Chunks are scheduled back-to-back from here rather than each at its own
  // absolute timestamp, so network-jitter bursts and clock-offset updates
  // don't create audible seams. Reset to 0 to force a re-anchor.
  let playHead = 0;
  let badSince = 0; // audioCtx time the cursor first went off-target (watchdog)
  let ctxRate = 48000; // audio context's native sample rate (set at join)
  const CHUNK_DUR = FRAMES_PER_CHUNK / SAMPLE_RATE; // 0.02s

  // Smoothed offset between the audio clock and performance.now() (seconds):
  // ctxTime ≈ perfMs/1000 + ctxPerfK. Reading audioContext.currentTime raw each
  // chunk is noisy on platforms where it advances in coarse steps (Linux /
  // Firefox), which jitters the play target; low-passing it keeps the target
  // stable. null until the first chunk anchors it; reset with playHead.
  let ctxPerfK = null;

  let muted = false;
  let wakeLock = null;

  // Auto-mute-on-drift: a speaker that can't trust its own sync FADES TO SILENCE
  // rather than playing out-of-phase audio (one out-of-sync speaker echo-ruins
  // the whole room; a silent one costs nothing). Driven by the Kalman filter's
  // own confidence (offset covariance), network jitter, corrections, and
  // interruptions. Fades hide the mute/unmute and also cover hard re-anchors.
  let syncReliable = true;
  let reliableSinceCtx = 0;   // audioCtx time healthy conditions began
  let unreliableUntilCtx = 0; // hold muted at least until this ctx time

  function updateOutputGain() {
    if (!muteGain || !audioCtx) return;
    const target = muted || !syncReliable ? 0 : 1;
    const t = audioCtx.currentTime;
    muteGain.gain.cancelScheduledValues(t);
    muteGain.gain.setValueAtTime(muteGain.gain.value, t);
    // ~150ms fade (setTargetAtTime settles in ~3 time constants).
    muteGain.gain.setTargetAtTime(target, t, 0.05);
  }

  // Called by the scheduler each chunk to (un)mute based on live confidence.
  function updateReliability(nowCtx) {
    const jitter = currentRtt - (minRtt || 0);
    const confident =
      offsetInit && Math.sqrt(kfP00) < 10 && jitter < 100;
    const healthy = confident && nowCtx >= unreliableUntilCtx;
    if (healthy) {
      if (reliableSinceCtx === 0) reliableSinceCtx = nowCtx;
      if (!syncReliable && nowCtx - reliableSinceCtx > 0.3) {
        syncReliable = true;
        updateOutputGain();
        if (joined) setStatus("synced", "synced");
      }
    } else {
      reliableSinceCtx = 0;
      if (syncReliable) {
        syncReliable = false;
        updateOutputGain();
        if (joined) setStatus("resyncing", "connecting");
      }
    }
  }

  // Mark the sync untrustworthy for at least `sec` (used around corrections and
  // interruptions) so output ducks out and back smoothly.
  function markUnreliable(sec) {
    if (!audioCtx) return;
    unreliableUntilCtx = Math.max(unreliableUntilCtx, audioCtx.currentTime + sec);
    if (syncReliable) {
      syncReliable = false;
      updateOutputGain();
      if (joined) setStatus("resyncing", "connecting");
    }
  }

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
    // Report our own measured RTT so the relay/master can show real per-device
    // latency and an accurate synced count.
    ws.send(JSON.stringify({ type: "ping", t0: performance.now(), rtt: Math.round(currentRtt) }));
  }

  function handlePong(msg) {
    const tRecv = performance.now();
    const rtt = tRecv - msg.t0;
    const z = msg.t1 + rtt / 2 - tRecv; // measured offset (ms), NTP-style

    // Rolling minimum-RTT baseline (snap down, slow leak up) — used to gauge how
    // much to trust each measurement.
    if (minRtt === null || rtt < minRtt) minRtt = rtt;
    else minRtt += Math.min(rtt - minRtt, 0.5);

    currentRtt = currentRtt === 0 ? rtt : currentRtt + (rtt - currentRtt) * 0.2;

    if (!offsetInit) {
      // Seed the filter from the first measurement.
      kfTheta = z;
      kfGamma = 0;
      kfP00 = 100; kfP01 = 0; kfP10 = 0; kfP11 = 1e-4;
      kfLastMs = tRecv;
      offsetInit = true;
      currentOffset = kfTheta;
      offsetVal.textContent = kfTheta.toFixed(1) + " ms";
      rttVal.textContent = currentRtt.toFixed(1) + " ms";
      setStatus("synced", "synced");
      return;
    }

    // --- Predict: advance state and covariance by tau ms since last update ---
    const tau = Math.max(1, tRecv - kfLastMs);
    kfLastMs = tRecv;
    kfTheta = kfTheta + kfGamma * tau; // theta += gamma*tau; gamma unchanged
    // P = F P F^T + Q, with F = [[1,tau],[0,1]]
    const a00 = kfP00 + tau * kfP10, a01 = kfP01 + tau * kfP11;
    const a10 = kfP10, a11 = kfP11;
    let p00 = a00 + a01 * tau, p01 = a01;
    let p10 = a10 + a11 * tau, p11 = a11;
    // Q: continuous white-noise-acceleration on the drift term.
    p00 += KF_Q * (tau * tau * tau) / 3;
    p01 += KF_Q * (tau * tau) / 2;
    p10 += KF_Q * (tau * tau) / 2;
    p11 += KF_Q * tau;

    // --- Update: fold in the measurement z (observes theta only) ---
    // Measurement noise R grows with how congested this ping was vs the clean
    // baseline: a low-jitter ping is trusted, a congested one is discounted.
    const jitter = Math.max(0, rtt - minRtt);
    const rStd = 1 + jitter / 2 + minRtt * 0.05;
    const R = rStd * rStd;
    const y = z - kfTheta; // innovation
    const S = p00 + R;
    const k0 = p00 / S;
    const k1 = p10 / S;
    kfTheta += k0 * y;
    kfGamma += k1 * y;
    kfP00 = (1 - k0) * p00;
    kfP01 = (1 - k0) * p01;
    kfP10 = p10 - k1 * p00;
    kfP11 = p11 - k1 * p01;

    currentOffset = kfTheta;
    offsetVal.textContent = kfTheta.toFixed(1) + " ms";
    rttVal.textContent = currentRtt.toFixed(1) + " ms";
    setStatus("synced", "synced");
  }

  // Offset extrapolated to `nowMs` using the estimated drift — smooth and
  // continuously drift-corrected between pings.
  function offsetNow(nowMs) {
    return offsetInit ? kfTheta + kfGamma * (nowMs - kfLastMs) : currentOffset;
  }

  // ---------------------------------------------------------------------
  // Binary audio frame handling
  // ---------------------------------------------------------------------

  function handleAudioFrame(buffer) {
    if (buffer.byteLength < FRAME_BYTES) return;
    const view = new DataView(buffer);

    const kind = view.getUint8(0);
    if (kind !== 0x01) return;

    // Decode s16 stereo and build the AudioBuffer AT THE CONTEXT'S NATIVE RATE,
    // resampling from 24kHz here in JS. Creating a 24kHz buffer and letting Web
    // Audio resample every 20ms chunk on playback is expensive on WebKit
    // (Safari / iOS) and was a stutter source; a cheap linear resample here is
    // far lighter. Buffer duration stays 20ms either way, so the scheduler math
    // is unchanged.
    const samples = new Int16Array(buffer, HEADER_BYTES, FRAMES_PER_CHUNK * 2);
    const outFrames = Math.max(1, Math.round((FRAMES_PER_CHUNK * ctxRate) / SAMPLE_RATE));
    const audioBuffer = audioCtx.createBuffer(2, outFrames, ctxRate);
    const chL = audioBuffer.getChannelData(0);
    const chR = audioBuffer.getChannelData(1);
    if (outFrames === FRAMES_PER_CHUNK) {
      for (let i = 0; i < FRAMES_PER_CHUNK; i++) {
        chL[i] = samples[i * 2] / 32768;
        chR[i] = samples[i * 2 + 1] / 32768;
      }
    } else {
      const step = SAMPLE_RATE / ctxRate; // input samples per output sample
      for (let i = 0; i < outFrames; i++) {
        const src = i * step;
        const i0 = src | 0;
        const frac = src - i0;
        const i1 = i0 + 1 < FRAMES_PER_CHUNK ? i0 + 1 : FRAMES_PER_CHUNK - 1;
        chL[i] = (samples[i0 * 2] * (1 - frac) + samples[i1 * 2] * frac) / 32768;
        chR[i] = (samples[i0 * 2 + 1] * (1 - frac) + samples[i1 * 2 + 1] * frac) / 32768;
      }
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
    const localPlayAtMs = playAt - offsetNow(nowMs);
    // Output-latency compensation: a scheduled sample isn't HEARD until it
    // clears the browser graph (baseLatency) and the OS/DAC/Bluetooth chain
    // (outputLatency) — up to ~200ms on Bluetooth. Schedule that much EARLIER so
    // every device's ACOUSTIC output lands on the shared target, not just its
    // logical schedule. Clamp so a bogus huge value can't push us past the buffer.
    const outDelay = Math.min(
      0.5,
      (audioCtx.baseLatency || 0) + (audioCtx.outputLatency || 0)
    );
    if (outLatVal) outLatVal.textContent = Math.round(outDelay * 1000) + " ms";
    const targetCtx = localPlayAtMs / 1000 + ctxPerfK - outDelay;

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
      // Any hard correction means we're momentarily NOT trustworthy: duck the
      // output so the jump is silent and we don't echo while re-locking.
      if (playHead !== 0) markUnreliable(0.4);
      badSince = 0;
      playHead = Math.max(targetCtx, now + 0.005);
    } else {
      // error > 0 → cursor ahead of target (buffer too deep) → play a hair
      // faster to drain; error < 0 → play a hair slower. Clamp to ±0.3%
      // (~5 cents, inaudible). Drives inter-device error to zero smoothly.
      const error = playHead - targetCtx;
      rate = Math.max(0.997, Math.min(1.003, 1 + 0.5 * error));
    }

    // Update auto-mute confidence each chunk (fades in once locked, out on drift).
    updateReliability(now);

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
      markUnreliable(1.0); // silent until re-synced after reconnect
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
    updateOutputGain();
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
    ctxRate = audioCtx.sampleRate || 48000; // build buffers at this rate

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
        markUnreliable(1.0); // stay silent until re-locked
        audioCtx.resume().catch(function () {});
      } else if (st === "running") {
        // reliability (and status) will recover via updateReliability once locked
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
