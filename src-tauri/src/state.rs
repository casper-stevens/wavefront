use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Overall mode of the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Idle,
    Master,
    Client,
    /// Hosting via a remote relay uplink instead of serving children directly.
    RelayHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientKind {
    Native,
    Browser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Sub,
    Tweeter,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pan {
    Left,
    Mid,
    Right,
}

#[derive(Debug, Clone, Copy, Serialize, serde::Deserialize)]
pub struct ClientConfig {
    pub role: Role,
    pub pan: Pan,
    pub gain: f32,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            role: Role::Full,
            pan: Pan::Mid,
            gain: 0.8,
        }
    }
}

/// Messages that can be pushed to a connected client's websocket task.
#[derive(Debug, Clone)]
pub enum ClientMsg {
    Config(ClientConfig),
    Disconnect,
}

/// Registry entry for a connected client (native app or browser tab).
pub struct ClientEntry {
    pub id: u32,
    pub name: String,
    pub kind: ClientKind,
    pub latency_ms: f64,
    pub config: ClientConfig,
    pub sender: mpsc::UnboundedSender<ClientMsg>,
}

#[derive(Serialize, Clone)]
pub struct ClientView {
    pub id: u32,
    pub name: String,
    pub kind: ClientKind,
    pub latency_ms: f64,
    pub config: ClientConfig,
}

impl From<&ClientEntry> for ClientView {
    fn from(c: &ClientEntry) -> Self {
        ClientView {
            id: c.id,
            name: c.name.clone(),
            kind: c.kind,
            latency_ms: c.latency_ms,
            config: c.config,
        }
    }
}

/// Registry entry for a child tracked via a relay uplink (no local socket —
/// the relay owns the actual connection). Populated from `child_joined` /
/// `roster` messages on the `/source` uplink; see relay_uplink.rs.
#[derive(Debug, Clone)]
pub struct RelayChild {
    pub id: u32,
    pub name: String,
    pub kind: ClientKind,
    pub latency_ms: f64,
    pub config: ClientConfig,
}

impl From<&RelayChild> for ClientView {
    fn from(c: &RelayChild) -> Self {
        ClientView {
            id: c.id,
            name: c.name.clone(),
            kind: c.kind,
            latency_ms: c.latency_ms,
            config: c.config,
        }
    }
}

/// Live status of this app's own client pipeline (native client mode).
#[derive(Debug, Clone, Serialize, Default)]
pub struct ClientStatus {
    pub connected: bool,
    pub master_addr: String,
    pub role: Role,
    pub pan: Pan,
    pub gain: f32,
    pub crossover_hz: f32,
    pub latency_ms: f64,
}

impl Default for Role {
    fn default() -> Self {
        Role::Full
    }
}

impl Default for Pan {
    fn default() -> Self {
        Pan::Mid
    }
}

/// Full shared application state.
pub struct AppState {
    pub mode: Mode,
    pub clients: HashMap<u32, ClientEntry>,
    pub next_client_id: u32,
    pub crossover_hz: f32,
    pub buffer_ms: u32,
    pub master_volume: f32,
    pub master_plays: bool,
    pub capture_source: String,
    pub warnings: Vec<String>,
    pub client_addr: Option<String>,
    pub client_status: Option<ClientStatus>,
    /// Children reported by a relay uplink (RelayHost mode), keyed by the
    /// relay-assigned id. Kept separate from `clients` so direct-LAN mode is
    /// untouched; merged into the view in `to_view()`.
    pub relay_children: HashMap<u32, RelayChild>,
    /// Public relay URL (e.g. "http://1.2.3.4:8927") to show as the join
    /// address while in RelayHost mode.
    pub relay_url: Option<String>,
    /// Raw JSON-line sender to the relay's `/source` socket, set by
    /// relay_uplink::run_uplink while connected. `set_client_config` /
    /// `set_crossover` write into this instead of per-client senders when
    /// `mode == RelayHost`.
    pub uplink_ctrl: Option<mpsc::UnboundedSender<String>>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            mode: Mode::Idle,
            clients: HashMap::new(),
            next_client_id: 1,
            crossover_hz: 220.0,
            buffer_ms: 250,
            master_volume: 1.0,
            master_plays: true,
            capture_source: String::new(),
            warnings: Vec::new(),
            client_addr: None,
            client_status: None,
            relay_children: HashMap::new(),
            relay_url: None,
            uplink_ctrl: None,
        }
    }
}

#[derive(Serialize, Clone)]
pub struct StateView {
    pub mode: Mode,
    pub clients: Vec<ClientView>,
    pub crossover_hz: f32,
    pub buffer_ms: u32,
    pub master_volume: f32,
    pub master_plays: bool,
    pub capture_source: String,
    pub warnings: Vec<String>,
    /// Join URL (http://<lan-ip>:8927) when hosting.
    pub addr: Option<String>,
    /// Network health, derived from client ping RTTs while hosting.
    pub wifi_ok: bool,
    /// Own client-pipeline status when in client mode.
    pub client: Option<ClientStatus>,
}

impl AppState {
    pub fn to_view(&self) -> StateView {
        let mut clients: Vec<ClientView> = self.clients.values().map(ClientView::from).collect();
        clients.extend(self.relay_children.values().map(ClientView::from));
        clients.sort_by_key(|c| c.id);
        let addr = match self.mode {
            Mode::Master => local_ip_address::local_ip()
                .ok()
                .map(|ip| format!("http://{ip}:8927")),
            Mode::RelayHost => self.relay_url.clone(),
            _ => None,
        };
        // Weak-network heuristic: any measured client RTT above 60 ms.
        let wifi_ok = clients
            .iter()
            .all(|c| c.latency_ms <= 0.0 || c.latency_ms < 60.0);
        StateView {
            mode: self.mode,
            clients,
            crossover_hz: self.crossover_hz,
            buffer_ms: self.buffer_ms,
            master_volume: self.master_volume,
            master_plays: self.master_plays,
            capture_source: self.capture_source.clone(),
            warnings: self.warnings.clone(),
            addr,
            wifi_ok,
            client: self.client_status.clone(),
        }
    }
}

pub type SharedState = Arc<parking_lot::Mutex<AppState>>;
