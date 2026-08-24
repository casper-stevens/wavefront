"use strict";
/*
 * Wavefront browser host. Captures tab/system audio via getDisplayMedia,
 * encodes to Opus (WebCodecs) — or 24kHz PCM as a fallback — and pushes it to
 * the relay's /source endpoint using the same wire frame the native master
 * sends (PROTOCOL.md): kind 0x01, flags (bit0=Opus), u16, f64 master_ts,
 * payload. The relay fans it out to all speakers. No install needed to host.
 */
(function () {
  const STREAM_RATE = 24000;      // wire rate (matches native master + clients)
  const FRAMES_PER_CHUNK = 480;   // 20ms @ 24kHz
  const CHUNK_MS = 20;

  const goBtn = document.getElementById("goBtn");
  const relayInput = document.getElementById("relay");
  const nameInput = document.getElementById("name");
  const statusPill = document.getElementById("statusPill");
  const stats = document.getElementById("stats");
  const codecVal = document.getElementById("codecVal");
  const sentVal = document.getElementById("sentVal");
  const brVal = document.getElementById("brVal");
  const joinUrl = document.getElementById("joinUrl");
  const hint = document.getElementById("hint");

  let hosting = false;
  let ws = null;
  let audioCtx = null;
  let displayStream = null;
  let workletNode = null;
  let srcNode = null;
  let encoder = null;      // WebCodecs AudioEncoder, or null for PCM fallback
  let acc = [];            // accumulated 48k stereo frames [l,r,...] pending chunking
  let accFrames = 0;
  let timelineMs = 0;      // uniform master timeline
  let chunkIndex = 0;
  let sent = 0;
  let bytesWindow = 0, brTimer = null;

  function setStatus(text, cls) {
    statusPill.textContent = text;
    statusPill.className = "pill " + cls;
  }

  function normalizeRelay(v) {
    v = (v || "").trim().replace(/\/+$/, "");
    let scheme = "ws", hostport = v;
    if (v.startsWith("https://")) { scheme = "wss"; hostport = v.slice(8); }
    else if (v.startsWith("http://")) { scheme = "ws"; hostport = v.slice(7); }
    if (scheme === "ws" && !/:\d+$/.test(hostport)) hostport += ":8927";
    return {
      ws: scheme + "://" + hostport + "/source",
      pub: (scheme === "wss" ? "https://" : "http://") + hostport,
    };
  }

  // AudioWorklet that just forwards captured input frames to the main thread.
  const CAPTURE_WORKLET = `
    class Cap extends AudioWorkletProcessor {
      process(inputs) {
        const inp = inputs[0];
        if (inp && inp[0]) {
          const l = inp[0], r = inp[1] || inp[0];
          const out = new Float32Array(l.length * 2);
          for (let i = 0; i < l.length; i++) { out[i*2] = l[i]; out[i*2+1] = r[i]; }
          this.port.postMessage(out, [out.buffer]);
        }
        return true;
      }
    }
    registerProcessor('wf-capture', Cap);
  `;

  async function start() {
    const { ws: wsUrl, pub } = normalizeRelay(relayInput.value);

    // Hard browser requirement: tab/screen audio capture needs a SECURE context
    // (HTTPS or localhost). Over plain http://<ip> it's simply unavailable.
    if (!window.isSecureContext || !navigator.mediaDevices || !navigator.mediaDevices.getDisplayMedia) {
      setStatus("needs HTTPS", "err");
      hint.innerHTML =
        "<b>Browser hosting needs HTTPS.</b> Tab/screen audio capture is blocked on plain http://. " +
        "Serve the relay over HTTPS (or run the host page from localhost) and reload. " +
        "Speakers still work over http — only the host needs a secure page.";
      return;
    }

    // 1) Capture tab/screen audio.
    try {
      displayStream = await navigator.mediaDevices.getDisplayMedia({
        video: true,
        audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false },
      });
    } catch (e) {
      setStatus("cancelled", "idle");
      return;
    }
    const audioTracks = displayStream.getAudioTracks();
    if (!audioTracks.length) {
      setStatus("no audio", "err");
      hint.innerHTML = "<b>No audio track.</b> You must tick <b>Share tab audio</b> / <b>Share system audio</b> in the picker. Stop and try again.";
      displayStream.getTracks().forEach((t) => t.stop());
      return;
    }
    // We don't need the video track — drop it.
    displayStream.getVideoTracks().forEach((t) => t.stop());

    audioCtx = new (window.AudioContext || window.webkitAudioContext)();
    const inRate = audioCtx.sampleRate; // usually 48000

    // 2) Set up the Opus encoder if WebCodecs is available, else PCM.
    if (typeof AudioEncoder !== "undefined") {
      try {
        encoder = new AudioEncoder({
          output: (chunk) => {
            const buf = new Uint8Array(chunk.byteLength);
            chunk.copyTo(buf);
            sendFrame(0x01, Number(chunk.timestamp) / 1000, buf);
          },
          error: () => {},
        });
        encoder.configure({ codec: "opus", sampleRate: STREAM_RATE, numberOfChannels: 2, bitrate: 64000 });
        codecVal.textContent = "Opus 64 kbps";
      } catch (e) {
        encoder = null;
      }
    }
    if (!encoder) codecVal.textContent = "PCM 24 kHz (WebCodecs unavailable)";

    // 3) Capture pipeline: source -> worklet -> (muted) destination.
    await audioCtx.audioWorklet.addModule(
      URL.createObjectURL(new Blob([CAPTURE_WORKLET], { type: "application/javascript" }))
    );
    srcNode = audioCtx.createMediaStreamSource(new MediaStream([audioTracks[0]]));
    workletNode = new AudioWorkletNode(audioCtx, "wf-capture");
    const mute = audioCtx.createGain();
    mute.gain.value = 0;
    srcNode.connect(workletNode).connect(mute).connect(audioCtx.destination);

    timelineMs = performance.now();
    chunkIndex = 0;
    acc = [];
    accFrames = 0;
    const ratio = inRate / STREAM_RATE; // input frames per output frame (~2)

    workletNode.port.onmessage = (e) => {
      const block = e.data; // interleaved stereo Float32 @ inRate
      const frames = block.length / 2;
      for (let i = 0; i < frames; i++) { acc.push(block[i * 2], block[i * 2 + 1]); }
      accFrames += frames;
      // Emit as many 20ms @24k chunks as we have input for.
      const inPerChunk = Math.round(FRAMES_PER_CHUNK * ratio);
      while (accFrames >= inPerChunk) {
        emitChunk(inPerChunk, ratio);
        acc.splice(0, inPerChunk * 2);
        accFrames -= inPerChunk;
      }
    };

    // 4) Connect to the relay source socket.
    ws = new WebSocket(wsUrl);
    ws.binaryType = "arraybuffer";
    ws.onopen = () => {
      ws.send(JSON.stringify({ type: "source_hello", buffer_ms: 1000, crossover_hz: 220 }));
      setStatus("live", "live");
      hosting = true;
      goBtn.textContent = "Stop hosting";
      goBtn.classList.add("stop");
      stats.classList.add("show");
      joinUrl.textContent = pub;
      hint.innerHTML = "Speakers open <b>" + pub + "</b> and tap Join. Keep this tab open while hosting.";
      brTimer = setInterval(() => {
        brVal.textContent = Math.round((bytesWindow * 8) / 1000) + " kbps";
        bytesWindow = 0;
      }, 1000);
    };
    ws.onclose = () => { if (hosting) stop(); };
    ws.onerror = () => { setStatus("relay error", "err"); };

    // Stop if the user ends the screen-share from the browser UI.
    audioTracks[0].onended = () => stop();
  }

  // Downsample one 20ms input span (inPerChunk frames @ inRate) to 480 @24k,
  // then hand it to the encoder (Opus) or ship it as PCM.
  function emitChunk(inPerChunk, ratio) {
    const L = new Float32Array(FRAMES_PER_CHUNK);
    const R = new Float32Array(FRAMES_PER_CHUNK);
    for (let i = 0; i < FRAMES_PER_CHUNK; i++) {
      const s = i * ratio;
      const i0 = s | 0;
      const frac = s - i0;
      const i1 = i0 + 1 < inPerChunk ? i0 + 1 : inPerChunk - 1;
      L[i] = acc[i0 * 2] * (1 - frac) + acc[i1 * 2] * frac;
      R[i] = acc[i0 * 2 + 1] * (1 - frac) + acc[i1 * 2 + 1] * frac;
    }

    const tsMs = timelineMs + chunkIndex * CHUNK_MS;
    chunkIndex++;

    if (encoder) {
      const data = new Float32Array(FRAMES_PER_CHUNK * 2);
      data.set(L, 0);
      data.set(R, FRAMES_PER_CHUNK);
      const ad = new AudioData({
        format: "f32-planar",
        sampleRate: STREAM_RATE,
        numberOfFrames: FRAMES_PER_CHUNK,
        numberOfChannels: 2,
        timestamp: Math.round(tsMs * 1000),
        data,
      });
      try { encoder.encode(ad); } catch (e) {}
      ad.close();
    } else {
      // PCM fallback: interleaved s16.
      const pcm = new Uint8Array(FRAMES_PER_CHUNK * 2 * 2);
      const dv = new DataView(pcm.buffer);
      for (let i = 0; i < FRAMES_PER_CHUNK; i++) {
        dv.setInt16(i * 4, Math.max(-1, Math.min(1, L[i])) * 32767, true);
        dv.setInt16(i * 4 + 2, Math.max(-1, Math.min(1, R[i])) * 32767, true);
      }
      sendFrame(0x00, tsMs, pcm);
    }
  }

  function sendFrame(flags, masterTsMs, payload) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const frame = new Uint8Array(12 + payload.length);
    const dv = new DataView(frame.buffer);
    dv.setUint8(0, 0x01);
    dv.setUint8(1, flags);
    dv.setUint16(2, 0, true);
    dv.setFloat64(4, masterTsMs, true);
    frame.set(payload, 12);
    ws.send(frame);
    sent++;
    sentVal.textContent = String(sent);
    bytesWindow += frame.length;
  }

  function stop() {
    hosting = false;
    try { if (ws) ws.close(); } catch (e) {}
    try { if (encoder && encoder.state !== "closed") encoder.close(); } catch (e) {}
    try { if (workletNode) workletNode.disconnect(); } catch (e) {}
    try { if (srcNode) srcNode.disconnect(); } catch (e) {}
    try { if (displayStream) displayStream.getTracks().forEach((t) => t.stop()); } catch (e) {}
    try { if (audioCtx) audioCtx.close(); } catch (e) {}
    if (brTimer) { clearInterval(brTimer); brTimer = null; }
    ws = encoder = workletNode = srcNode = displayStream = audioCtx = null;
    setStatus("idle", "idle");
    goBtn.textContent = "Start hosting";
    goBtn.classList.remove("stop");
    goBtn.disabled = false;
  }

  goBtn.addEventListener("click", async () => {
    if (hosting) { stop(); return; }
    goBtn.disabled = true;
    setStatus("starting…", "idle");
    try {
      await start();
    } catch (e) {
      setStatus("error", "err");
      hint.textContent = "Failed to start: " + (e && e.message ? e.message : e);
    }
    goBtn.disabled = false;
  });
})();
