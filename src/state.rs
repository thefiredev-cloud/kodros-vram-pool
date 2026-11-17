//! Shared runtime state: devices, backends, health, memory, rates.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::breaker::Breaker;
use crate::config::{BackendCfg, Config, DeviceCfg, Role};
use crate::fleet::Fleet;
use crate::swap::SwapCoordinator;
use crate::util::now_iso;

pub struct Pool {
    pub cfg: Config,
    pub devices: Vec<Arc<Device>>,
    pub backends: Vec<Arc<Backend>>,
    /// Client for proxied traffic: connect timeout only. Header and idle timeouts are applied per request.
    pub http: reqwest::Client,
    /// Client for probes and Fleet Studio calls.
    pub probe_http: reqwest::Client,
    pub fleet: Option<Fleet>,
    pub swap: SwapCoordinator,
    pub started: Instant,
    pub http_requests: Mutex<BTreeMap<(String, u16), u64>>,
}

impl Pool {
    pub fn new(cfg: Config) -> Result<Arc<Self>, String> {
        let s = &cfg.server;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(s.connect_timeout_s))
            // No idle reuse: llama.cpp closes idle connections after ~5 s, and a request that lands
            // on a connection the server just closed fails for no real reason.
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let probe_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(s.probe_timeout_s))
            .timeout(Duration::from_secs(s.probe_timeout_s))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| format!("probe client: {e}"))?;
        let devices: Vec<Arc<Device>> = cfg
            .devices
            .iter()
            .cloned()
            .map(|c| Arc::new(Device::new(c)))
            .collect();
        let backends: Vec<Arc<Backend>> = cfg
            .backends
            .iter()
            .map(|b| {
                let idx = cfg
                    .devices
                    .iter()
                    .position(|d| d.id == b.device)
                    .unwrap_or(0);
                Arc::new(Backend::new(b.clone(), idx, s))
            })
            .collect();
        let fleet = cfg
            .fleet_studio
            .as_ref()
            .map(|f| Fleet::new(&f.url, probe_http.clone()));
        Ok(Arc::new(Self {
            swap: SwapCoordinator::new(cfg.swap.clone()),
            cfg,
            devices,
            backends,
            http,
            probe_http,
            fleet,
            started: Instant::now(),
            http_requests: Mutex::new(BTreeMap::new()),
        }))
    }

    pub fn device_of(&self, b: &Backend) -> &Arc<Device> {
        &self.devices[b.device]
    }

    pub fn backends_of<'a>(&'a self, device: usize) -> impl Iterator<Item = &'a Arc<Backend>> + 'a {
        self.backends.iter().filter(move |b| b.device == device)
    }

    pub fn count_http(&self, path: &str, status: u16) {
        let mut g = lock(&self.http_requests);
        *g.entry((path.to_owned(), status)).or_insert(0) += 1;
    }
}

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn read<T>(m: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn write<T>(m: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone)]
pub struct GpuApp {
    pub pid: u32,
    pub name: String,
    pub used_mib: u64,
}

#[derive(Debug, Clone, Default)]
pub struct MemState {
    pub total_mib: Option<u64>,
    pub used_mib: Option<u64>,
    pub free_mib: Option<u64>,
    pub ok: bool,
    pub error: Option<String>,
    pub checked_at: Option<String>,
    pub apps: Vec<GpuApp>,
    pub load1: Option<f64>,
}

pub struct Device {
    pub cfg: DeviceCfg,
    pub memory: RwLock<MemState>,
}

impl Device {
    pub fn new(cfg: DeviceCfg) -> Self {
        let mut m = MemState::default();
        if cfg.memory.source == crate::config::MemorySource::Static {
            m = crate::probe::static_mem(cfg.memory.total_mib, cfg.memory.used_mib);
            m.ok = true;
        }
        Self {
            cfg,
            memory: RwLock::new(m),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Up,
    Down,
    /// Slot with nothing loaded. Not a failure: a hot-swap can fill it.
    Idle,
}

impl HealthState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Health {
    pub state: HealthState,
    pub reason: Option<String>,
    pub latency_ms: Option<u64>,
    pub checked_at: Option<String>,
    pub since: String,
}

impl Health {
    fn initial() -> Self {
        Self {
            state: HealthState::Down,
            reason: Some("not probed yet".to_owned()),
            latency_ms: None,
            checked_at: None,
            since: now_iso(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    pub ctx: Option<u64>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Stats {
    stamps: VecDeque<Instant>,
    pub last_tok_s: Option<f64>,
    pub last_tok_at: Option<Instant>,
    pub ewma_ms: Option<f64>,
    pub ok: u64,
    pub failed: u64,
    pub by_status: BTreeMap<u16, u64>,
}

const RATE_WINDOW: Duration = Duration::from_secs(30);

impl Stats {
    pub fn note_request(&mut self, now: Instant) {
        self.stamps.push_back(now);
        self.trim(now);
    }

    fn trim(&mut self, now: Instant) {
        while let Some(front) = self.stamps.front() {
            if now.duration_since(*front) > RATE_WINDOW {
                self.stamps.pop_front();
            } else {
                break;
            }
        }
    }

    /// Requests per second over the last 30 s (or since start when younger).
    pub fn rps(&mut self, now: Instant, since_start: Duration) -> f64 {
        self.trim(now);
        let span = since_start.min(RATE_WINDOW).as_secs_f64().max(1.0);
        self.stamps.len() as f64 / span
    }

    pub fn note_latency(&mut self, ms: f64) {
        self.ewma_ms = Some(match self.ewma_ms {
            Some(prev) => prev * 0.7 + ms * 0.3,
            None => ms,
        });
    }
}

pub struct Backend {
    pub cfg: BackendCfg,
    pub device: usize,
    pub key: crate::keys::KeyStore,
    pub health: RwLock<Health>,
    pub models: RwLock<Vec<ModelEntry>>,
    pub breaker: Breaker,
    pub inflight: AtomicUsize,
    pub stats: Mutex<Stats>,
}

impl Backend {
    pub fn new(cfg: BackendCfg, device: usize, s: &crate::config::ServerCfg) -> Self {
        Self {
            key: crate::keys::KeyStore::new(cfg.key_file.clone(), cfg.key_ssh.clone()),
            cfg,
            device,
            health: RwLock::new(Health::initial()),
            models: RwLock::new(Vec::new()),
            breaker: Breaker::new(
                s.breaker_failures,
                Duration::from_secs(s.breaker_open_s),
                Duration::from_secs(s.breaker_max_open_s),
            ),
            inflight: AtomicUsize::new(0),
            stats: Mutex::new(Stats::default()),
        }
    }

    pub fn id(&self) -> &str {
        &self.cfg.id
    }

    pub fn role(&self) -> Role {
        self.cfg.role
    }

    pub fn is_up(&self) -> bool {
        read(&self.health).state == HealthState::Up
    }

    pub fn state(&self) -> HealthState {
        read(&self.health).state
    }

    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    /// The entry this backend serves under `model`: exact id first, else the same model by
    /// normalized name (so `qwen3-1.7b-q4_k_m` finds `slot-4080/Qwen3-1.7B-Q4_K_M`).
    pub fn serves(&self, model: &str) -> Option<ModelEntry> {
        let models = read(&self.models);
        if let Some(m) = models.iter().find(|m| m.id == model) {
            return Some(m.clone());
        }
        let want = crate::util::norm_model(model);
        models
            .iter()
            .find(|m| crate::util::norm_model(&m.id) == want)
            .cloned()
    }

    pub fn model_ids(&self) -> Vec<String> {
        read(&self.models).iter().map(|m| m.id.clone()).collect()
    }

    pub fn last_tok_s(&self) -> Option<f64> {
        lock(&self.stats).last_tok_s
    }

    pub fn ewma_ms(&self) -> Option<f64> {
        lock(&self.stats).ewma_ms
    }

    /// Sets health and reports whether the state changed.
    pub fn set_health(
        &self,
        state: HealthState,
        reason: Option<String>,
        latency_ms: Option<u64>,
    ) -> Option<(HealthState, HealthState)> {
        let mut h = write(&self.health);
        let prev = h.state;
        h.checked_at = Some(now_iso());
        h.latency_ms = latency_ms;
        h.reason = reason;
        if prev != state {
            h.state = state;
            h.since = now_iso();
            Some((prev, state))
        } else {
            None
        }
    }
}

/// Keeps `inflight` accurate on every exit path, including a client dropping a stream.
pub struct InflightGuard {
    backend: Arc<Backend>,
}

impl InflightGuard {
    pub fn new(backend: Arc<Backend>) -> Self {
        backend.inflight.fetch_add(1, Ordering::Relaxed);
        Self { backend }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.backend.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}
