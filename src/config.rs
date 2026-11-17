//! Configuration: `~/.config/vram-pool/pool.toml`.
//!
//! The service only ever binds a loopback address. `validate` rejects `0.0.0.0`, `::` and any
//! non-loopback address, so a config typo cannot expose the pool.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

pub const DEFAULT_BIND: &str = "127.0.0.1:8100";

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerCfg,
    #[serde(default)]
    pub fleet_studio: Option<FleetStudioCfg>,
    #[serde(default)]
    pub swap: SwapCfg,
    #[serde(default, rename = "alias")]
    pub aliases: Vec<AliasCfg>,
    #[serde(default, rename = "device")]
    pub devices: Vec<DeviceCfg>,
    #[serde(default, rename = "backend")]
    pub backends: Vec<BackendCfg>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub bind: String,
    /// Seconds between backend health probes.
    pub health_interval_s: u64,
    /// Seconds between device memory probes.
    pub memory_interval_s: u64,
    /// Timeout of one health or model-list probe.
    pub probe_timeout_s: u64,
    /// TCP connect timeout for proxied requests.
    pub connect_timeout_s: u64,
    /// Seconds to wait for response headers from a backend (non-streaming completions send headers last).
    pub request_timeout_s: u64,
    /// Seconds a streaming response may stay silent before the pool cuts it.
    pub stream_idle_timeout_s: u64,
    /// Total tries per request, each on a different backend. Never more than one try per backend.
    pub max_attempts: usize,
    /// Consecutive failures that open a backend's circuit.
    pub breaker_failures: u32,
    /// First open period in seconds. Doubles on repeated half-open failures up to `breaker_max_open_s`.
    pub breaker_open_s: u64,
    pub breaker_max_open_s: u64,
    /// Serve `GET /metrics` (Prometheus text).
    pub metrics: bool,
    /// Largest request body in MiB.
    pub max_body_mib: usize,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            bind: DEFAULT_BIND.to_owned(),
            health_interval_s: 5,
            memory_interval_s: 10,
            probe_timeout_s: 3,
            connect_timeout_s: 3,
            request_timeout_s: 120,
            stream_idle_timeout_s: 60,
            max_attempts: 3,
            breaker_failures: 3,
            breaker_open_s: 20,
            breaker_max_open_s: 120,
            metrics: true,
            max_body_mib: 64,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct FleetStudioCfg {
    /// Base URL of the Fleet Studio API, for example `http://127.0.0.1:18090`.
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SwapCfg {
    pub enabled: bool,
    /// Context size requested for a swapped-in model.
    pub ctx: u32,
    /// Fleet Studio's own preflight factor and overhead, mirrored here to refuse early.
    pub vram_factor: f64,
    pub overhead_gib: f64,
    /// VRAM that must stay free after a swap, for the other residents of the GPU (other services, the desktop).
    pub reserve_mib: u64,
    /// Seconds to wait for Fleet Studio to finish one swap (its load timeout is 300 s).
    pub timeout_s: u64,
    /// After a failed or refused swap of a model, do not ask again for this long.
    pub cooldown_s: u64,
}

impl Default for SwapCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            ctx: 8192,
            vram_factor: 1.15,
            overhead_gib: 0.6,
            reserve_mib: 1536,
            timeout_s: 330,
            cooldown_s: 120,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AliasCfg {
    pub name: String,
    /// Ordered model ids. The first resident one wins; later ones are fallbacks.
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceKind {
    Gpu,
    Unified,
    Cluster,
}

impl DeviceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Unified => "unified",
            Self::Cluster => "cluster",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCfg {
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub kind: DeviceKind,
    #[serde(default)]
    pub memory: MemoryCfg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemorySource {
    /// `nvidia-smi` on this machine.
    NvidiaSmi,
    /// `sysctl hw.memsize` and `memory_pressure` over ssh on a Mac.
    SshMacos,
    /// Fixed `total_mib`; used and free are unknown.
    Static,
    /// The device cannot report and no size is configured.
    #[default]
    None,
}

impl MemorySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NvidiaSmi => "nvidia-smi",
            Self::SshMacos => "ssh-macos",
            Self::Static => "static",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MemoryCfg {
    pub source: MemorySource,
    /// Required for `static`; ignored by probed sources.
    pub total_mib: Option<u64>,
    /// Optional for `static`: a known fixed usage, which makes free = total - used.
    pub used_mib: Option<u64>,
    /// ssh alias for `ssh-macos`.
    pub ssh: Option<String>,
    /// GPU index for `nvidia-smi`. All GPUs are summed when absent.
    pub gpu_index: Option<u32>,
    /// Free-text note shown in `/pool/status`, for example where a static number came from.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Always-on chat model.
    Chat,
    /// Hot-swap slot: chat model that Fleet Studio can replace.
    Slot,
    /// Decision model (judge, router). Listed, never given chat traffic.
    Decision,
    /// Embedding model.
    Embedding,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Slot => "slot",
            Self::Decision => "decision",
            Self::Embedding => "embedding",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelsSource {
    /// `GET /v1/models`.
    #[default]
    V1Models,
    /// The `model` field of the health JSON (for gateways without `/v1/models`).
    HealthModel,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackendCfg {
    pub id: String,
    pub device: String,
    pub url: String,
    pub role: Role,
    #[serde(default)]
    pub engine: String,
    #[serde(default = "default_health_path")]
    pub health_path: String,
    #[serde(default)]
    pub models_source: ModelsSource,
    /// Bearer key file on this machine (first non-empty line, kept in memory only).
    #[serde(default)]
    pub key_file: Option<String>,
    /// Bearer key fetched over ssh: `{ host = "mac-host", path = "/abs/path" }`. Kept in memory only.
    #[serde(default)]
    pub key_ssh: Option<KeySsh>,
    /// Context size to report when the backend does not.
    #[serde(default)]
    pub ctx: Option<u64>,
    /// Model ids shown (flagged unavailable) while the backend is down.
    #[serde(default)]
    pub models: Vec<String>,
    /// Fleet Studio host id of this slot (`rtx4080`). Only `role = "slot"` backends swap.
    #[serde(default)]
    pub fleet_host: Option<String>,
    /// Fleet Studio host id used only to explain why this backend is down (`cluster`).
    #[serde(default)]
    pub explain_via_fleet: Option<String>,
    /// Free-text note shown in `/pool/status`.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct KeySsh {
    pub host: String,
    pub path: String,
}

fn default_health_path() -> String {
    "/health".to_owned()
}

impl Config {
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join(".config/vram-pool/pool.toml")
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| format!("config parse: {e}"))?;
        cfg.normalize();
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    fn normalize(&mut self) {
        for d in &mut self.devices {
            if d.label.is_empty() {
                d.label = d.id.clone();
            }
        }
        for b in &mut self.backends {
            b.url = b.url.trim_end_matches('/').to_owned();
            if b.engine.is_empty() {
                b.engine = "openai-compatible".to_owned();
            }
        }
    }

    /// The parsed bind address. Always loopback after `validate`.
    pub fn bind_addr(&self) -> Result<SocketAddr, String> {
        self.server
            .bind
            .parse::<SocketAddr>()
            .map_err(|e| format!("server.bind {:?}: {e}", self.server.bind))
    }

    pub fn validate(&self) -> Result<(), String> {
        let addr = self.bind_addr()?;
        if !addr.ip().is_loopback() {
            return Err(format!(
                "server.bind {addr}: refused. vram-pool binds loopback only (127.0.0.1 or ::1), never 0.0.0.0"
            ));
        }
        let s = &self.server;
        if s.health_interval_s == 0 || s.memory_interval_s == 0 || s.probe_timeout_s == 0 {
            return Err("server intervals and probe_timeout_s must be at least 1".to_owned());
        }
        if s.max_attempts == 0 || s.breaker_failures == 0 {
            return Err("server.max_attempts and breaker_failures must be at least 1".to_owned());
        }
        let mut seen = std::collections::HashSet::new();
        for d in &self.devices {
            check_id(&d.id)?;
            if !seen.insert(d.id.as_str()) {
                return Err(format!("duplicate device id {:?}", d.id));
            }
            match d.memory.source {
                MemorySource::Static if d.memory.total_mib.is_none() => {
                    return Err(format!(
                        "device {}: memory.source=static needs total_mib",
                        d.id
                    ));
                }
                MemorySource::SshMacos => match &d.memory.ssh {
                    Some(h) if shell_safe(h) => {}
                    _ => return Err(format!("device {}: memory.ssh missing or unsafe", d.id)),
                },
                _ => {}
            }
        }
        let mut bseen = std::collections::HashSet::new();
        for b in &self.backends {
            check_id(&b.id)?;
            if !bseen.insert(b.id.as_str()) {
                return Err(format!("duplicate backend id {:?}", b.id));
            }
            if !self.devices.iter().any(|d| d.id == b.device) {
                return Err(format!("backend {}: unknown device {:?}", b.id, b.device));
            }
            if !(b.url.starts_with("http://") || b.url.starts_with("https://")) {
                return Err(format!(
                    "backend {}: url must start with http:// or https://",
                    b.id
                ));
            }
            if !b.health_path.starts_with('/') {
                return Err(format!("backend {}: health_path must start with /", b.id));
            }
            if let Some(k) = &b.key_ssh
                && !(shell_safe(&k.host) && shell_safe(&k.path))
            {
                return Err(format!(
                    "backend {}: key_ssh host or path has unsafe characters",
                    b.id
                ));
            }
            if b.role == Role::Slot && b.fleet_host.is_none() && self.swap.enabled {
                // A slot without a Fleet Studio host can still serve; it just never swaps.
                tracing::debug!(backend = %b.id, "slot has no fleet_host; hot-swap disabled for it");
            }
        }
        for a in &self.aliases {
            if a.name.is_empty() || a.models.is_empty() {
                return Err("alias needs a name and at least one model".to_owned());
            }
        }
        if self.swap.enabled
            && self.backends.iter().any(|b| b.fleet_host.is_some())
            && self.fleet_studio.is_none()
        {
            return Err("a backend sets fleet_host but [fleet_studio] url is missing".to_owned());
        }
        Ok(())
    }
}

fn check_id(id: &str) -> Result<(), String> {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(format!("id {id:?} must be 1-64 chars of [A-Za-z0-9._-]"))
    }
}

/// Characters allowed in values that reach a remote shell through ssh.
pub fn shell_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'+' | b'@' | b':')
        })
}

/// Expands a leading `~/`.
pub fn expand(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// Default config written by `vram-pool --print-default`. An example layout; replace it with your own devices.
pub const DEFAULT_TOML: &str = include_str!("../deploy/pool.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = Config::parse(DEFAULT_TOML).expect("default config parses");
        assert_eq!(cfg.server.bind, DEFAULT_BIND);
        assert!(cfg.devices.len() >= 4);
    }

    #[test]
    fn refuses_wildcard_bind() {
        for bind in [
            "0.0.0.0:8100",
            "[::]:8100",
            "203.0.113.7:8100",
            "192.168.1.5:8100",
        ] {
            let text = format!("[server]\nbind = \"{bind}\"\n");
            let err = Config::parse(&text).expect_err(bind);
            assert!(err.contains("refused"), "{bind}: {err}");
        }
        assert!(Config::parse("[server]\nbind = \"127.0.0.1:9\"\n").is_ok());
        assert!(Config::parse("[server]\nbind = \"[::1]:9\"\n").is_ok());
    }

    #[test]
    fn rejects_unknown_device_and_unsafe_ssh() {
        let bad = r#"
[[device]]
id = "a"
kind = "gpu"
[[backend]]
id = "b"
device = "nope"
url = "http://127.0.0.1:1"
role = "chat"
"#;
        assert!(Config::parse(bad).unwrap_err().contains("unknown device"));
        let bad2 = r#"
[[device]]
id = "a"
kind = "unified"
memory = { source = "ssh-macos", ssh = "mac; rm -rf /" }
"#;
        assert!(Config::parse(bad2).unwrap_err().contains("unsafe"));
    }
}
