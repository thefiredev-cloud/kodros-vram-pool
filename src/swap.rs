//! Hot-swap decision: ask Fleet Studio to load a library model into the 4080 slot when the
//! requested model is resident nowhere and the slot has room.
//!
//! One swap runs at a time (single flight): concurrent requests for the same model wait on the
//! lock and find the model resident afterwards. A refused or failed swap of a model is not
//! retried for `cooldown_s`, so a bad request cannot hammer Fleet Studio.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::config::{Role, SwapCfg};
use crate::fleet::{LibEntry, SWAPPABLE_HOST};
use crate::state::{Pool, lock, read};
use crate::util::{MIB, norm_model as norm, now_iso, one_line};

#[derive(Debug, Clone)]
pub struct SwapRecord {
    pub at: String,
    pub model: String,
    pub file: Option<String>,
    /// `ok`, `refused` (pool declined) or `failed` (Fleet Studio declined or errored).
    pub result: &'static str,
    pub detail: String,
    pub ms: u64,
}

pub struct SwapCoordinator {
    pub cfg: SwapCfg,
    gate: tokio::sync::Mutex<()>,
    cooldown: Mutex<HashMap<String, (Instant, String, bool)>>,
    pub last: Mutex<Option<SwapRecord>>,
    pub requested: AtomicU64,
    pub succeeded: AtomicU64,
    pub failed: AtomicU64,
}

impl SwapCoordinator {
    pub fn new(cfg: SwapCfg) -> Self {
        Self {
            cfg,
            gate: tokio::sync::Mutex::new(()),
            cooldown: Mutex::new(HashMap::new()),
            last: Mutex::new(None),
            requested: AtomicU64::new(0),
            succeeded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }

    /// `(reason, model_is_a_library_file)` while a model is in cooldown.
    fn cooling(&self, model: &str) -> Option<(String, bool)> {
        let g = lock(&self.cooldown);
        let (at, why, known) = g.get(model)?;
        let left = Duration::from_secs(self.cfg.cooldown_s).checked_sub(at.elapsed())?;
        Some((
            format!(
                "swap of {model} paused {}s more after: {why}",
                left.as_secs() + 1
            ),
            *known,
        ))
    }

    fn start_cooldown(&self, model: &str, why: &str, known: bool) {
        lock(&self.cooldown).insert(
            model.to_owned(),
            (Instant::now(), one_line(why, 160), known),
        );
    }

    fn record(&self, rec: SwapRecord) {
        *lock(&self.last) = Some(rec);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SwapOutcome {
    /// A swap ran and the slot now serves the model.
    Swapped,
    /// Another request finished a swap (or the model came back) while this one waited.
    AlreadyResident,
    /// The model is a library file but cannot be loaded now (no room, Fleet Studio said no, cooldown).
    Declined(String),
    /// No swap happened and the model is not a known library file; the reason is for logs and errors.
    Skipped(String),
}

/// Finds the library file a model id refers to: exact (case, `slot-4080/` prefix and `.gguf`
/// ignored), else the single file whose name starts with `<id>-` (a quant suffix).
pub fn match_library<'a>(model: &str, lib: &'a [LibEntry]) -> Option<&'a LibEntry> {
    let want = norm(model);
    if let Some(e) = lib.iter().find(|e| norm(&e.name) == want) {
        return Some(e);
    }
    let prefix = format!("{want}-");
    let mut hits = lib.iter().filter(|e| norm(&e.name).starts_with(&prefix));
    match (hits.next(), hits.next()) {
        (Some(e), None) => Some(e),
        _ => None,
    }
}

/// MiB a model file needs on the GPU, by Fleet Studio's own formula.
pub fn need_mib(size_bytes: u64, factor: f64, overhead_gib: f64) -> u64 {
    (size_bytes as f64 / MIB * factor + overhead_gib * 1024.0).ceil() as u64
}

pub async fn try_swap(pool: &Arc<Pool>, model: &str) -> SwapOutcome {
    let sw = &pool.swap;
    if !sw.cfg.enabled {
        return SwapOutcome::Skipped("hot-swap disabled in config".to_owned());
    }
    let Some(fleet) = pool.fleet.as_ref() else {
        return SwapOutcome::Skipped("no [fleet_studio] configured".to_owned());
    };
    let Some(slot) = pool
        .backends
        .iter()
        .find(|b| b.role() == Role::Slot && b.cfg.fleet_host.as_deref() == Some(SWAPPABLE_HOST))
    else {
        return SwapOutcome::Skipped("no swap slot configured".to_owned());
    };

    let _single_flight = sw.gate.lock().await;
    if pool
        .backends
        .iter()
        .any(|b| b.is_up() && b.serves(model).is_some())
    {
        return SwapOutcome::AlreadyResident;
    }
    if let Some((why, known)) = sw.cooling(model) {
        return if known {
            SwapOutcome::Declined(why)
        } else {
            SwapOutcome::Skipped(why)
        };
    }
    let started = Instant::now();
    sw.requested.fetch_add(1, Ordering::Relaxed);

    // `file` is Some only once the model matched a library file: then the model exists, it just
    // cannot be loaded now, and the caller answers 503 instead of 404.
    let refuse = |why: String, file: Option<String>, cool: bool| {
        let known = file.is_some();
        if cool {
            sw.start_cooldown(model, &why, known);
        }
        sw.record(SwapRecord {
            at: now_iso(),
            model: model.to_owned(),
            file,
            result: "refused",
            detail: one_line(&why, 200),
            ms: started.elapsed().as_millis() as u64,
        });
        if known {
            SwapOutcome::Declined(why)
        } else {
            SwapOutcome::Skipped(why)
        }
    };

    let view = match fleet.slot_view(SWAPPABLE_HOST).await {
        Ok(v) => v,
        Err(e) => return refuse(e, None, true),
    };
    if view.swap_in_progress {
        return refuse(
            "another swap is running on the slot".to_owned(),
            None,
            false,
        );
    }
    match &view.url {
        Some(u) if u.trim_end_matches('/') == slot.cfg.url => {}
        other => {
            return refuse(
                format!(
                    "fleet studio slot url {other:?} differs from configured {}",
                    slot.cfg.url
                ),
                None,
                true,
            );
        }
    }
    if slot.inflight() > 0 {
        return refuse(
            format!("slot has {} request(s) in flight", slot.inflight()),
            None,
            false,
        );
    }
    let lib = match fleet.library(SWAPPABLE_HOST).await {
        Ok(l) => l,
        Err(e) => return refuse(e, None, true),
    };
    let Some(entry) = match_library(model, &lib) else {
        return refuse(
            format!(
                "model {model:?} is not in the 4080 library ({} files)",
                lib.len()
            ),
            None,
            true,
        );
    };

    let need = need_mib(entry.size_bytes, sw.cfg.vram_factor, sw.cfg.overhead_gib);
    let dev = pool.device_of(slot);
    let free = read(&dev.memory).free_mib;
    // The swap unloads what the slot holds first, so that memory comes back.
    let reclaim: u64 = read(&slot.models)
        .iter()
        .filter(|_| slot.is_up())
        .filter_map(|m| m.size_bytes)
        .map(|s| need_mib(s, sw.cfg.vram_factor, sw.cfg.overhead_gib))
        .sum();
    let reserve = sw.cfg.reserve_mib;
    if let Some(free) = free
        && need + reserve > free + reclaim
    {
        return refuse(
            format!(
                "{} needs {need} MiB plus a {reserve} MiB safety reserve, {free} MiB free (+{reclaim} MiB held by the slot)",
                entry.name
            ),
            Some(entry.name.clone()),
            true,
        );
    }

    tracing::info!(model, file = %entry.name, need_mib = need, "requesting hot-swap through fleet studio");
    let result = fleet
        .swap(
            SWAPPABLE_HOST,
            &entry.name,
            sw.cfg.ctx,
            Duration::from_secs(sw.cfg.timeout_s),
        )
        .await;
    let ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(_) => {
            crate::probe::probe_backend(pool, slot).await;
            if let Some(d) = pool.devices.get(slot.device) {
                crate::probe::probe_memory(d).await;
            }
            sw.succeeded.fetch_add(1, Ordering::Relaxed);
            sw.record(SwapRecord {
                at: now_iso(),
                model: model.to_owned(),
                file: Some(entry.name.clone()),
                result: "ok",
                detail: format!("slot reloaded in {ms} ms"),
                ms,
            });
            if slot.is_up() {
                SwapOutcome::Swapped
            } else {
                SwapOutcome::Declined(
                    "swap finished but the slot did not come up healthy".to_owned(),
                )
            }
        }
        Err(e) => {
            sw.failed.fetch_add(1, Ordering::Relaxed);
            sw.start_cooldown(model, &e.message, true);
            sw.record(SwapRecord {
                at: now_iso(),
                model: model.to_owned(),
                file: Some(entry.name.clone()),
                result: "failed",
                detail: one_line(&e.message, 200),
                ms,
            });
            SwapOutcome::Declined(e.message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> Vec<LibEntry> {
        vec![
            LibEntry {
                name: "Qwen3-1.7B-Q4_K_M.gguf".into(),
                size_bytes: 1_107_409_472,
            },
            LibEntry {
                name: "Qwen3.5-2B-Q4_K_M.gguf".into(),
                size_bytes: 1_280_835_840,
            },
            LibEntry {
                name: "Qwen3.5-4B-Q8_0.gguf".into(),
                size_bytes: 4_482_403_488,
            },
        ]
    }

    #[test]
    fn library_matching() {
        let l = lib();
        assert_eq!(
            match_library("Qwen3-1.7B-Q4_K_M.gguf", &l).map(|e| e.name.as_str()),
            Some("Qwen3-1.7B-Q4_K_M.gguf")
        );
        assert_eq!(
            match_library("slot-4080/qwen3-1.7b-q4_k_m", &l).map(|e| e.size_bytes),
            Some(1_107_409_472)
        );
        assert_eq!(
            match_library("qwen3-1.7b", &l).map(|e| e.name.as_str()),
            Some("Qwen3-1.7B-Q4_K_M.gguf")
        );
        assert!(
            match_library("qwen3.5", &l).is_none(),
            "ambiguous prefix must not guess"
        );
        assert!(match_library("llama-3", &l).is_none());
    }

    #[test]
    fn need_matches_fleet_studio_formula() {
        // 1.107e9 B = 1056 MiB; * 1.15 + 0.6 GiB (614.4 MiB) = about 1829 MiB.
        let n = need_mib(1_107_409_472, 1.15, 0.6);
        assert!((1820..1840).contains(&n), "{n}");
    }
}
