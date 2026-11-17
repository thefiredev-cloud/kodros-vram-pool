//! Read-only Fleet Studio client plus the one write it is allowed: a slot swap on the RTX 4080.
//!
//! Fleet Studio owns the hot-swap slot. The pool only calls:
//! - `GET  /api/status`                      library, slot state, why the cluster is down
//! - `POST /api/hosts/rtx4080/slot/swap`     `{ "model": <library file name>, "ctx": <int> }`
//!
//! Nothing else is ever sent, and the swap host is fixed to `rtx4080` here, not in config.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::state::lock;

/// The only Fleet Studio host the pool may swap on (the 127.0.0.1:8846 slot).
pub const SWAPPABLE_HOST: &str = "rtx4080";
const STATUS_TTL: Duration = Duration::from_secs(10);
const STATUS_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq)]
pub struct LibEntry {
    pub name: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SlotView {
    /// `running`, `stopped`, `swapping`, ... as Fleet Studio reports it.
    pub state: String,
    pub url: Option<String>,
    pub file: Option<String>,
    pub swap_in_progress: bool,
}

#[derive(Debug)]
pub struct SwapError {
    pub status: Option<u16>,
    pub message: String,
}

pub struct Fleet {
    base: String,
    http: reqwest::Client,
    cache: Mutex<Option<(Instant, Value)>>,
    flight: tokio::sync::Mutex<()>,
}

impl Fleet {
    pub fn new(base: &str, http: reqwest::Client) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            http,
            cache: Mutex::new(None),
            flight: tokio::sync::Mutex::new(()),
        }
    }

    /// `GET /api/status`, cached for 10 s with a single in-flight refresh.
    pub async fn status(&self, force: bool) -> Result<Value, String> {
        if !force
            && let Some((at, v)) = lock(&self.cache).clone()
            && at.elapsed() < STATUS_TTL
        {
            return Ok(v);
        }
        let _one = self.flight.lock().await;
        if !force
            && let Some((at, v)) = lock(&self.cache).clone()
            && at.elapsed() < STATUS_TTL
        {
            return Ok(v);
        }
        let url = format!("{}/api/status", self.base);
        let resp = self
            .http
            .get(&url)
            .timeout(STATUS_TIMEOUT)
            .send()
            .await
            .map_err(|e| {
                format!(
                    "fleet studio unreachable: {}",
                    crate::util::describe_http_error(&e, STATUS_TIMEOUT)
                )
            })?;
        if !resp.status().is_success() {
            return Err(format!(
                "fleet studio /api/status returned HTTP {}",
                resp.status().as_u16()
            ));
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| format!("fleet studio status is not JSON: {e}"))?;
        *lock(&self.cache) = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    fn host<'a>(status: &'a Value, id: &str) -> Option<&'a Value> {
        status
            .get("hosts")?
            .as_array()?
            .iter()
            .find(|h| h.get("id").and_then(Value::as_str) == Some(id))
    }

    pub async fn library(&self, host: &str) -> Result<Vec<LibEntry>, String> {
        let st = self.status(false).await?;
        let h =
            Self::host(&st, host).ok_or_else(|| format!("fleet studio has no host {host:?}"))?;
        Ok(h.get("library")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| {
                        Some(LibEntry {
                            name: e.get("name")?.as_str()?.to_owned(),
                            size_bytes: e.get("size")?.as_u64()?,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Slot state, always fresh (a swap decision must not use a 10 s old answer).
    pub async fn slot_view(&self, host: &str) -> Result<SlotView, String> {
        let st = self.status(true).await?;
        let h =
            Self::host(&st, host).ok_or_else(|| format!("fleet studio has no host {host:?}"))?;
        let slot = h
            .get("slot")
            .ok_or_else(|| format!("fleet studio host {host:?} has no slot"))?;
        Ok(SlotView {
            state: slot
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            url: slot.get("url").and_then(Value::as_str).map(str::to_owned),
            file: slot.get("file").and_then(Value::as_str).map(str::to_owned),
            swap_in_progress: slot.get("swap").is_some_and(|s| !s.is_null()),
        })
    }

    /// Why a Fleet Studio host is down, in one line. `None` when Fleet Studio does not say.
    pub async fn explain(&self, host: &str) -> Option<String> {
        let st = self.status(false).await.ok()?;
        let h = Self::host(&st, host)?;
        let mut parts: Vec<String> = Vec::new();
        if let Some(e) = h.get("error").and_then(Value::as_str) {
            parts.push(crate::util::one_line(e, 160));
        }
        if let Some(nodes) = h.get("nodes").and_then(Value::as_array) {
            for n in nodes {
                if n.get("online").and_then(Value::as_bool) == Some(false) {
                    let label = n.get("label").and_then(Value::as_str).unwrap_or("node");
                    let err = n.get("error").and_then(Value::as_str).unwrap_or("offline");
                    parts.push(format!("{label}: {}", crate::util::one_line(err, 100)));
                }
            }
        }
        if let Some(s) = h.get("summary").and_then(Value::as_str) {
            parts.push(s.to_owned());
        }
        (!parts.is_empty()).then(|| format!("fleet studio: {}", parts.join("; ")))
    }

    /// Requests one swap on the 4080 slot and waits for Fleet Studio to finish it.
    pub async fn swap(
        &self,
        host: &str,
        model_file: &str,
        ctx: u32,
        timeout: Duration,
    ) -> Result<Value, SwapError> {
        if host != SWAPPABLE_HOST {
            return Err(SwapError {
                status: None,
                message: format!("swap refused: host {host:?} is not {SWAPPABLE_HOST}"),
            });
        }
        let url = format!("{}/api/hosts/{host}/slot/swap", self.base);
        let body = json!({ "model": model_file, "ctx": ctx }).to_string();
        let resp = self
            .http
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| SwapError {
                status: None,
                message: format!(
                    "fleet studio swap call failed: {}",
                    crate::util::describe_http_error(&e, timeout)
                ),
            })?;
        let status = resp.status().as_u16();
        let v: Value = resp.json().await.unwrap_or(Value::Null);
        // Any answer invalidates the cached status: the slot changed or was refused for a reason.
        *lock(&self.cache) = None;
        if (200..300).contains(&status) && v.get("ok").and_then(Value::as_bool) != Some(false) {
            Ok(v)
        } else {
            let why = v
                .get("reason")
                .or_else(|| v.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("no reason given");
            Err(SwapError {
                status: Some(status),
                message: format!(
                    "fleet studio refused swap (HTTP {status}): {}",
                    crate::util::one_line(why, 200)
                ),
            })
        }
    }
}
