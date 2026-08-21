// Wavefront UI logic.
//
// Runs either inside Tauri (window.__TAURI__ present, "app > withGlobalTauri": true)
// or in a plain browser for demoing (mock mode, using sample data modeled on the
// mockup at spotify_sync/mockup/wavefront.html).
//
// Command surface (see PROTOCOL.md "Master UI <-> backend" and the "Relay
// extension (v1)" appendix):
//   invoke: start_master, start_relay_host{relay}, stop_master,
//           set_client_config{id,role,pan,gain}, set_crossover{hz},
//           set_master_volume{v}, set_master_plays{on}, start_client{addr},
//           stop_client, get_status
//   event:  "wavefront://state" -> full serialized state, emitted on every
//           change and at 1 Hz.
//
// Assumed state shape (protocol doesn't pin exact field names, only "mode,
// clients list with id/name/kind/latency/config, capture source label,
// warnings"). We use:
// {
//   mode: "idle" | "master" | "relayhost" | "client",
//   addr: "192.168.1.23:8927",           // LAN join address (master mode),
//                                         // or the relay URL (relayhost mode)
//   capture_label: "Broadcasting System Audio",
//   master_plays: false,
//   master_volume: 0.78,                  // 0..1
//   crossover_hz: 220,
//   wifi_ok: true,
//   warnings: ["..."],                    // free-text warnings from backend
//   clients: [
//     { id, name, kind: "app"|"browser", latency_ms, synced,
//       role: "sub"|"tweeter"|"full", pan: "left"|"mid"|"right", gain: 0..1 }
//   ],
//   client: {                             // present in client mode
//     connected: true, master_addr: "192.168.1.10:8927",
//     role, pan, gain, crossover_hz, latency_ms
//   }
// }

(function () {
  "use strict";

  const TAURI = window.__TAURI__ && window.__TAURI__.core ? window.__TAURI__ : null;

  // ---------------------------------------------------------------------
  // Backend abstraction
  // ---------------------------------------------------------------------

  function makeTauriBackend() {
    const invoke = (cmd, args) => TAURI.core.invoke(cmd, args);
    return {
      mock: false,
      startMaster: () => invoke("start_master"),
      startRelayHost: (relay) => invoke("start_relay_host", { relay }),
      stopMaster: () => invoke("stop_master"),
      startClient: (addr) => invoke("start_client", { addr }),
      stopClient: () => invoke("stop_client"),
      setClientConfig: (id, role, pan, gain) => invoke("set_client_config", { id, role, pan, gain }),
      setCrossover: (hz) => invoke("set_crossover", { hz }),
      setMasterVolume: (v) => invoke("set_master_volume", { v }),
      setMasterPlays: (on) => invoke("set_master_plays", { on }),
      getStatus: () => invoke("get_status"),
      onState: (cb) => {
        TAURI.event.listen("wavefront://state", (evt) => cb(evt.payload));
      },
    };
  }

  function makeMockBackend() {
    let state = {
      mode: "idle",
      addr: "192.168.1.23:8927",
      capture_label: "Broadcasting System Audio",
      master_plays: false,
      master_volume: 0.78,
      crossover_hz: 220,
      wifi_ok: true,
      warnings: [],
      clients: [
        { id: "1", name: "Dorm Sub — Left", kind: "app", latency_ms: 6, synced: true, role: "sub", pan: "left", gain: 0.82 },
        { id: "2", name: "Casper's Air", kind: "app", latency_ms: 4, synced: true, role: "tweeter", pan: "mid", gain: 0.70 },
        { id: "3", name: "Mila's Pro", kind: "browser", latency_ms: 5, synced: true, role: "tweeter", pan: "mid", gain: 0.65 },
        { id: "4", name: "Hallway — Mono", kind: "app", latency_ms: 8, synced: true, role: "full", pan: "mid", gain: 0.55 },
        { id: "5", name: "Balcony Tweeter", kind: "browser", latency_ms: 11, synced: false, role: "tweeter", pan: "mid", gain: 0.60 },
      ],
      client: null,
    };

    let listeners = [];
    function emit() {
      const copy = JSON.parse(JSON.stringify(state));
      listeners.forEach((cb) => cb(copy));
    }

    // Simulate periodic 1 Hz state emission with tiny latency jitter, like the real backend.
    setInterval(() => {
      if (state.mode === "master") {
        state.clients.forEach((c) => {
          if (c.synced) {
            c.latency_ms = Math.max(2, c.latency_ms + (Math.random() * 2 - 1));
          }
        });
      }
      emit();
    }, 1000);

    return {
      mock: true,
      startMaster: () => {
        state.mode = "master";
        emit();
        return Promise.resolve();
      },
      startRelayHost: (relay) => {
        state.mode = "relayhost";
        state.addr = relay ? "http://" + relay.replace(/^https?:\/\//, "") : "http://relay.example:8927";
        emit();
        return Promise.resolve();
      },
      stopMaster: () => {
        state.mode = "idle";
        emit();
        return Promise.resolve();
      },
      startClient: (addr) => {
        state.mode = "client";
        state.client = {
          connected: true,
          master_addr: addr,
          role: "tweeter",
          pan: "mid",
          gain: 0.7,
          crossover_hz: 220,
          latency_ms: 5,
        };
        emit();
        return Promise.resolve();
      },
      stopClient: () => {
        state.mode = "idle";
        state.client = null;
        emit();
        return Promise.resolve();
      },
      setClientConfig: (id, role, pan, gain) => {
        const c = state.clients.find((c) => c.id === id);
        if (c) {
          if (role !== undefined) c.role = role;
          if (pan !== undefined) c.pan = pan;
          if (gain !== undefined) c.gain = gain;
        }
        emit();
        return Promise.resolve();
      },
      setCrossover: (hz) => {
        state.crossover_hz = hz;
        emit();
        return Promise.resolve();
      },
      setMasterVolume: (v) => {
        state.master_volume = v;
        emit();
        return Promise.resolve();
      },
      setMasterPlays: (on) => {
        state.master_plays = on;
        emit();
        return Promise.resolve();
      },
      getStatus: () => Promise.resolve(JSON.parse(JSON.stringify(state))),
      onState: (cb) => {
        listeners.push(cb);
      },
    };
  }

  const backend = TAURI ? makeTauriBackend() : makeMockBackend();

  // ---------------------------------------------------------------------
  // DOM refs
  // ---------------------------------------------------------------------

  const screens = {
    chooser: document.getElementById("screen-chooser"),
    dashboard: document.getElementById("screen-dashboard"),
    client: document.getElementById("screen-client"),
  };

  function showScreen(name) {
    Object.entries(screens).forEach(([k, el]) => el.classList.toggle("active", k === name));
  }

  // Chooser
  const cardHost = document.getElementById("cardHost");
  const cardJoin = document.getElementById("cardJoin");
  const joinForm = document.getElementById("joinForm");
  const joinAddr = document.getElementById("joinAddr");
  const joinGo = document.getElementById("joinGo");
  const hostGo = document.getElementById("hostGo");
  const hostRelayForm = document.getElementById("hostRelayForm");
  const hostRelayAddr = document.getElementById("hostRelayAddr");
  const chooserError = document.getElementById("chooserError");

  // Dashboard
  const captureLabel = document.getElementById("captureLabel");
  const masterVolumeEl = document.getElementById("masterVolume");
  const masterRoleToggle = document.getElementById("masterRoleToggle");
  const joinUrlChip = document.getElementById("joinUrlChip");
  const joinUrlText = document.getElementById("joinUrlText");
  const wifiChip = document.getElementById("wifiChip");
  const wifiLabel = document.getElementById("wifiLabel");
  const stopMasterBtn = document.getElementById("stopMasterBtn");
  const warnBanner = document.getElementById("warnBanner");
  const warnText = document.getElementById("warnText");
  const deviceList = document.getElementById("deviceList");
  const onlineCount = document.getElementById("onlineCount");
  const room = document.getElementById("room");
  const xoverCanvas = document.getElementById("xoverCanvas");
  const xoverSlider = document.getElementById("xoverSlider");
  const xoverReadout = document.getElementById("xoverReadout");
  const syncDot = document.getElementById("syncDot");
  const syncSummary = document.getElementById("syncSummary");
  const netSummary = document.getElementById("netSummary");

  // Client screen
  const clientStateIcon = document.getElementById("clientStateIcon");
  const clientTitle = document.getElementById("clientTitle");
  const clientSub = document.getElementById("clientSub");
  const clientRole = document.getElementById("clientRole");
  const clientPan = document.getElementById("clientPan");
  const clientGain = document.getElementById("clientGain");
  const clientLatency = document.getElementById("clientLatency");
  const stopClientBtn = document.getElementById("stopClientBtn");

  // ---------------------------------------------------------------------
  // Mode chooser
  // ---------------------------------------------------------------------

  let chooserMode = null; // "host" | "join"

  cardHost.addEventListener("click", () => {
    chooserMode = "host";
    cardHost.classList.add("selected");
    cardJoin.classList.remove("selected");
    joinForm.classList.remove("show");
    hostRelayForm.classList.add("show");
    hostGo.style.display = "inline-block";
    chooserError.textContent = "";
  });

  cardJoin.addEventListener("click", () => {
    chooserMode = "join";
    cardJoin.classList.add("selected");
    cardHost.classList.remove("selected");
    joinForm.classList.add("show");
    hostRelayForm.classList.remove("show");
    hostGo.style.display = "none";
    chooserError.textContent = "";
    joinAddr.focus();
  });

  hostGo.addEventListener("click", () => {
    chooserError.textContent = "";
    const relay = hostRelayAddr.value.trim();
    const start = relay ? backend.startRelayHost(relay) : backend.startMaster();
    start.catch((e) => {
      chooserError.textContent = "Could not start hosting: " + describeError(e);
    });
  });

  function doJoin() {
    const addr = joinAddr.value.trim();
    if (!addr) {
      chooserError.textContent = "Enter the master's address.";
      return;
    }
    chooserError.textContent = "";
    backend.startClient(addr).catch((e) => {
      chooserError.textContent = "Could not join: " + describeError(e);
    });
  }
  joinGo.addEventListener("click", doJoin);
  joinAddr.addEventListener("keydown", (e) => {
    if (e.key === "Enter") doJoin();
  });

  function describeError(e) {
    if (typeof e === "string") return e;
    if (e && e.message) return e.message;
    return "unknown error";
  }

  // ---------------------------------------------------------------------
  // Toolbar controls (dashboard)
  // ---------------------------------------------------------------------

  stopMasterBtn.addEventListener("click", () => backend.stopMaster());
  stopClientBtn.addEventListener("click", () => backend.stopClient());

  masterRoleToggle.addEventListener("change", () => {
    backend.setMasterPlays(masterRoleToggle.checked);
  });

  const debouncedMasterVolume = debounce((v) => backend.setMasterVolume(v), 100);
  masterVolumeEl.addEventListener("input", () => {
    debouncedMasterVolume(parseInt(masterVolumeEl.value, 10) / 100);
  });

  joinUrlChip.addEventListener("click", () => {
    const url = joinUrlText.textContent;
    if (navigator.clipboard && url && url !== "http://—") {
      navigator.clipboard.writeText(url).catch(() => {});
    }
  });

  const debouncedCrossover = debounce((hz) => backend.setCrossover(hz), 100);
  xoverSlider.addEventListener("input", () => {
    const val = parseInt(xoverSlider.value, 10);
    xoverReadout.textContent = val;
    drawCrossover(freqToNorm(val, xoverSlider));
    debouncedCrossover(val);
  });

  function debounce(fn, ms) {
    let t = null;
    let lastArgs = null;
    return (...args) => {
      lastArgs = args;
      if (t) return;
      t = setTimeout(() => {
        t = null;
        fn(...lastArgs);
      }, ms);
    };
  }

  // ---------------------------------------------------------------------
  // Crossover canvas (identical curve math to the mockup)
  // ---------------------------------------------------------------------

  const xctx = xoverCanvas.getContext("2d");

  function styleVar(name) {
    return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  }

  function freqToNorm(val, slider) {
    const min = parseFloat(slider.min), max = parseFloat(slider.max);
    return 0.08 + ((val - min) / (max - min)) * 0.7;
  }

  function drawCrossover(freqNorm) {
    const w = xoverCanvas.width, h = xoverCanvas.height;
    xctx.clearRect(0, 0, w, h);
    const low = styleVar("--low"), high = styleVar("--high"), grid = styleVar("--divider");

    xctx.strokeStyle = grid;
    xctx.lineWidth = 1;
    for (let i = 1; i < 3; i++) {
      const y = (h / 3) * i;
      xctx.beginPath(); xctx.moveTo(0, y); xctx.lineTo(w, y); xctx.stroke();
    }

    const cx = freqNorm * w;

    xctx.strokeStyle = low; xctx.lineWidth = 2;
    xctx.beginPath();
    for (let x = 0; x <= w; x++) {
      const t = (x - cx) / (w * 0.18);
      const y = h * 0.52 - (h * 0.36) / (1 + Math.exp(t));
      if (x === 0) xctx.moveTo(x, y); else xctx.lineTo(x, y);
    }
    xctx.stroke();

    xctx.strokeStyle = high; xctx.lineWidth = 2;
    xctx.beginPath();
    for (let x = 0; x <= w; x++) {
      const t = (x - cx) / (w * 0.18);
      const y = h * 0.52 - (h * 0.36) / (1 + Math.exp(-t));
      if (x === 0) xctx.moveTo(x, y); else xctx.lineTo(x, y);
    }
    xctx.stroke();

    xctx.strokeStyle = styleVar("--text-tertiary");
    xctx.setLineDash([3, 3]); xctx.lineWidth = 1;
    xctx.beginPath(); xctx.moveTo(cx, 0); xctx.lineTo(cx, h); xctx.stroke();
    xctx.setLineDash([]);
  }

  // ---------------------------------------------------------------------
  // Room layout — local-only puck positions, persisted per client name
  // ---------------------------------------------------------------------

  const ROOM_POS_KEY = "wavefront.roomPositions";

  function loadPositions() {
    try {
      return JSON.parse(localStorage.getItem(ROOM_POS_KEY) || "{}");
    } catch (e) {
      return {};
    }
  }
  function savePositions(positions) {
    try {
      localStorage.setItem(ROOM_POS_KEY, JSON.stringify(positions));
    } catch (e) {
      /* ignore */
    }
  }
  let roomPositions = loadPositions();

  function defaultPositionFor(index, total) {
    // Spread new pucks in a loose ring around the room, deterministic per index.
    const angle = (index / Math.max(total, 1)) * Math.PI * 2 - Math.PI / 2;
    const x = 50 + Math.cos(angle) * 36;
    const y = 50 + Math.sin(angle) * 36;
    return { x: Math.max(6, Math.min(94, x)), y: Math.max(6, Math.min(94, y)) };
  }

  const roleIconSvg = {
    sub: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 9v6h3l5 4V5L7 9H4Z"/></svg>',
    tweeter: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 9v6h3l5 4V5L7 9H4Z"/><path d="M16 9a4 4 0 0 1 0 6"/></svg>',
    full: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 9v6h3l5 4V5L7 9H4Z"/><path d="M16 9a4 4 0 0 1 0 6M18.5 6.5a8 8 0 0 1 0 11"/></svg>',
  };
  const masterIconSvg = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="12" rx="2"/><path d="M2 20h20"/></svg>';

  const roleClassFor = { sub: "low", tweeter: "high", full: "full" };
  const roleLabelFor = { sub: "Sub", tweeter: "Tweeter", full: "Full‑range" };
  const panLabelFor = { left: "L", mid: "M", right: "R" };

  let dragEl = null;
  let dragKey = null;

  room.addEventListener("mousedown", (e) => {
    const puck = e.target.closest(".puck");
    if (!puck) return;
    dragEl = puck;
    dragKey = puck.dataset.posKey;
    puck.style.zIndex = 10;
    e.preventDefault();
  });
  document.addEventListener("mousemove", (e) => {
    if (!dragEl) return;
    const r = room.getBoundingClientRect();
    let x = ((e.clientX - r.left) / r.width) * 100;
    let y = ((e.clientY - r.top) / r.height) * 100;
    x = Math.max(4, Math.min(96, x));
    y = Math.max(4, Math.min(96, y));
    dragEl.style.left = x + "%";
    dragEl.style.top = y + "%";
  });
  document.addEventListener("mouseup", () => {
    if (dragEl && dragKey) {
      const left = parseFloat(dragEl.style.left);
      const top = parseFloat(dragEl.style.top);
      roomPositions[dragKey] = { x: left, y: top };
      savePositions(roomPositions);
    }
    dragEl = null;
    dragKey = null;
  });

  function renderRoom(clients, masterPlays) {
    // Preserve the master puck plus one puck per client, keyed by name.
    room.querySelectorAll(".puck").forEach((p) => p.remove());

    const keys = clients.map((c) => c.name);
    clients.forEach((c, i) => {
      const key = c.name;
      let pos = roomPositions[key];
      if (!pos) {
        pos = defaultPositionFor(i, clients.length);
        roomPositions[key] = pos;
      }
      const roleClass = roleClassFor[c.role] || "full";
      const puck = document.createElement("div");
      puck.className = "puck " + roleClass;
      puck.dataset.posKey = key;
      puck.style.left = pos.x + "%";
      puck.style.top = pos.y + "%";
      puck.innerHTML =
        '<div class="ring">' + (roleIconSvg[c.role] || roleIconSvg.full) + "</div>" +
        '<div class="pname">' + escapeHtml(c.name) + "</div>";
      room.appendChild(puck);
    });

    const masterKey = "__master__";
    let mpos = roomPositions[masterKey];
    if (!mpos) {
      mpos = { x: 50, y: 50 };
      roomPositions[masterKey] = mpos;
    }
    const masterPuck = document.createElement("div");
    masterPuck.className = "puck master" + (masterPlays ? "" : " off");
    masterPuck.dataset.posKey = masterKey;
    masterPuck.style.left = mpos.x + "%";
    masterPuck.style.top = mpos.y + "%";
    masterPuck.innerHTML =
      '<div class="ring">' + masterIconSvg + "</div>" +
      '<div class="pname">This Mac</div>';
    room.appendChild(masterPuck);

    savePositions(roomPositions);
  }

  function escapeHtml(s) {
    const d = document.createElement("div");
    d.textContent = s;
    return d.innerHTML;
  }

  // ---------------------------------------------------------------------
  // Sidebar device list
  // ---------------------------------------------------------------------

  const gainDebouncers = {}; // per-client id -> debounced setter (~10/s)

  // The backend command needs the full config every time, so every setter
  // sends role+pan+gain together, merged from the client's last-known state.
  function gainDebouncerFor(id) {
    if (!gainDebouncers[id]) {
      gainDebouncers[id] = debounce((role, pan, gain) => backend.setClientConfig(id, role, pan, gain), 100);
    }
    return gainDebouncers[id];
  }

  function renderDeviceList(clients, masterPlays) {
    onlineCount.textContent = clients.length + " online";

    deviceList.innerHTML = "";

    const masterRow = document.createElement("div");
    masterRow.className = "device-row is-master";
    masterRow.innerHTML =
      '<div class="dr-top">' +
      '<div class="role-icon master">' + masterIconSvg + "</div>" +
      '<div class="dr-name"><div class="n1">This Mac — Master</div>' +
      '<div class="n2">' + (masterPlays ? "Full‑range · in room" : "Broadcasting only") + "</div></div>" +
      "</div>";
    deviceList.appendChild(masterRow);

    clients.forEach((c) => {
      const row = document.createElement("div");
      row.className = "device-row" + (c.synced ? "" : " is-connecting");

      const roleClass = roleClassFor[c.role] || "full";
      const kindLabel = c.kind === "browser" ? "browser" : "app";

      row.innerHTML =
        '<div class="dr-top">' +
        '<div class="role-icon ' + roleClass + '">' + (roleIconSvg[c.role] || roleIconSvg.full) + "</div>" +
        '<div class="dr-name"><div class="n1">' + escapeHtml(c.name) + '</div>' +
        '<div class="n2">' + roleLabelFor[c.role] + " · " + Math.round(c.latency_ms) + " ms</div></div>" +
        '<span class="kind-badge">' + kindLabel + "</span>" +
        '<span class="status-dot' + (c.synced ? "" : " warn") + '"></span>' +
        "</div>" +
        '<div class="dr-body">' +
        '<div class="segmented">' +
        '<button data-role="sub"' + (c.role === "sub" ? ' class="active"' : "") + ">Sub</button>" +
        '<button data-role="tweeter"' + (c.role === "tweeter" ? ' class="active"' : "") + ">Tweeter</button>" +
        '<button data-role="full"' + (c.role === "full" ? ' class="active"' : "") + ">Full</button>" +
        "</div>" +
        '<div class="pan-row">' +
        '<span class="pl">Pan</span>' +
        '<div class="segmented">' +
        '<button data-pan="left"' + (c.pan === "left" ? ' class="active"' : "") + ">L</button>" +
        '<button data-pan="mid"' + (c.pan === "mid" ? ' class="active"' : "") + ">M</button>" +
        '<button data-pan="right"' + (c.pan === "right" ? ' class="active"' : "") + ">R</button>" +
        "</div>" +
        "</div>" +
        '<div class="gain-row">' +
        '<span class="gl mono">' + Math.round(c.gain * 100) + '</span>' +
        '<input type="range" min="0" max="100" value="' + Math.round(c.gain * 100) + '">' +
        "</div>" +
        "</div>";

      row.querySelectorAll(".segmented button[data-role]").forEach((btn) => {
        btn.addEventListener("click", () => {
          row.querySelectorAll(".segmented button[data-role]").forEach((b) => b.classList.remove("active"));
          btn.classList.add("active");
          c.role = btn.dataset.role;
          backend.setClientConfig(c.id, c.role, c.pan, c.gain);
        });
      });
      row.querySelectorAll(".segmented button[data-pan]").forEach((btn) => {
        btn.addEventListener("click", () => {
          row.querySelectorAll(".segmented button[data-pan]").forEach((b) => b.classList.remove("active"));
          btn.classList.add("active");
          c.pan = btn.dataset.pan;
          backend.setClientConfig(c.id, c.role, c.pan, c.gain);
        });
      });
      const gainInput = row.querySelector(".gain-row input[type=range]");
      const gainLabel = row.querySelector(".gain-row .gl");
      gainInput.addEventListener("input", () => {
        gainLabel.textContent = gainInput.value;
        c.gain = parseInt(gainInput.value, 10) / 100;
        gainDebouncerFor(c.id)(c.role, c.pan, c.gain);
      });

      deviceList.appendChild(row);
    });
  }

  // ---------------------------------------------------------------------
  // Full dashboard render from state
  // ---------------------------------------------------------------------

  let xoverInitialized = false;

  function renderDashboard(state) {
    captureLabel.textContent = state.capture_label || "Broadcasting System Audio";

    if (document.activeElement !== masterVolumeEl) {
      masterVolumeEl.value = Math.round((state.master_volume ?? 0.78) * 100);
    }

    masterRoleToggle.checked = !!state.master_plays;

    const addr = state.addr || "";
    joinUrlText.textContent = addr ? "http://" + addr : "http://—";

    wifiChip.classList.toggle("weak", state.wifi_ok === false);
    wifiLabel.textContent = state.wifi_ok === false ? "Wi‑Fi weak" : "Wi‑Fi strong";

    const warnings = state.warnings || [];
    if (warnings.length) {
      warnBanner.classList.add("show");
      warnText.innerHTML = "<b>" + escapeHtml(warnings[0]) + "</b>" +
        (warnings.length > 1 ? ' <span class="bsub">+' + (warnings.length - 1) + " more</span>" : "");
    } else {
      warnBanner.classList.remove("show");
    }

    const clients = state.clients || [];
    renderDeviceList(clients, state.master_plays);
    renderRoom(clients, state.master_plays);

    if (!xoverInitialized || document.activeElement !== xoverSlider) {
      const hz = state.crossover_hz ?? 220;
      xoverSlider.value = hz;
      xoverReadout.textContent = hz;
      drawCrossover(freqToNorm(hz, xoverSlider));
      xoverInitialized = true;
    }

    const syncedCount = clients.filter((c) => c.synced).length;
    syncDot.style.background = syncedCount === clients.length ? "var(--good)" : "var(--warn)";
    syncSummary.textContent = syncedCount + " of " + clients.length + " speakers synced";
    netSummary.textContent = addr ? addr.split(":")[0] + " · no device limit — scales with your Wi‑Fi" : "";
  }

  // ---------------------------------------------------------------------
  // Client screen render
  // ---------------------------------------------------------------------

  function renderClient(state) {
    const c = state.client || {};
    if (c.connected) {
      clientStateIcon.classList.remove("disconnected");
      clientTitle.textContent = "Connected";
      clientSub.textContent = "Joined " + (c.master_addr || "master session");
    } else {
      clientStateIcon.classList.add("disconnected");
      clientTitle.textContent = "Disconnected";
      clientSub.textContent = "Lost connection to " + (c.master_addr || "master");
    }
    clientRole.textContent = roleLabelFor[c.role] || "—";
    clientPan.textContent = panLabelFor[c.pan] || "—";
    clientGain.textContent = c.gain !== undefined ? Math.round(c.gain * 100) + "%" : "—";
    clientLatency.textContent = c.latency_ms !== undefined ? Math.round(c.latency_ms) + " ms" : "—";
  }

  // ---------------------------------------------------------------------
  // State dispatch
  // ---------------------------------------------------------------------

  // The Rust backend nests per-client config and uses kind "native"; flatten
  // and rename into the shape the renderers use. Mock-backend states already
  // have the flat shape and pass through unchanged.
  function normalizeState(s) {
    if (!s) return s;
    const clients = (s.clients || []).map((c) => ({
      id: c.id,
      name: c.name,
      kind: c.kind === "native" ? "app" : c.kind,
      latency_ms: c.latency_ms || 0,
      synced: c.synced !== undefined ? c.synced : (c.latency_ms || 0) > 0 && c.latency_ms < 60,
      role: c.role !== undefined ? c.role : c.config ? c.config.role : "full",
      pan: c.pan !== undefined ? c.pan : c.config ? c.config.pan : "mid",
      gain: c.gain !== undefined ? c.gain : c.config ? c.config.gain : 0.8,
    }));
    return Object.assign({}, s, {
      clients,
      capture_label:
        s.capture_label || (s.capture_source ? "Capturing: " + s.capture_source : ""),
      wifi_ok: s.wifi_ok !== false,
    });
  }

  function applyState(rawState) {
    const state = normalizeState(rawState);
    if (!state) return;
    if (state.mode === "master" || state.mode === "relayhost") {
      showScreen("dashboard");
      renderDashboard(state);
    } else if (state.mode === "client") {
      showScreen("client");
      renderClient(state);
    } else {
      showScreen("chooser");
    }
  }

  backend.onState(applyState);

  // Kick off with current status if available (covers app relaunch mid-session).
  if (backend.getStatus) {
    backend.getStatus().then(applyState).catch(() => {});
  }
})();
