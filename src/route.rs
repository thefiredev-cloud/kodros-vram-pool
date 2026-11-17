//! Candidate selection: which backends may take a request, best first.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use crate::breaker::CircuitState;
use crate::config::Role;
use crate::state::{Backend, Pool};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Chat,
    Completions,
    Embeddings,
}

impl Endpoint {
    pub fn path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Completions => "/v1/completions",
            Self::Embeddings => "/v1/embeddings",
        }
    }

    /// Roles that may serve this endpoint. Decision models never take chat traffic.
    pub fn allows(self, role: Role) -> bool {
        match self {
            Self::Chat | Self::Completions => matches!(role, Role::Chat | Role::Slot),
            Self::Embeddings => matches!(role, Role::Embedding | Role::Chat | Role::Slot),
        }
    }
}

#[derive(Clone)]
pub struct Cand {
    pub backend: Arc<Backend>,
    /// The model id this backend serves it under.
    pub served_id: String,
}

/// Healthy backends serving `model`, best first: fewest in-flight requests, then lowest measured
/// latency, then highest last tok/s. Backends in `tried` and backends with an open circuit are left out.
pub fn ranked(pool: &Pool, ep: Endpoint, model: &str, tried: &HashSet<String>) -> Vec<Cand> {
    let now = Instant::now();
    let mut out: Vec<Cand> = pool
        .backends
        .iter()
        .filter(|b| ep.allows(b.role()) && b.is_up() && !tried.contains(b.id()))
        .filter(|b| b.breaker.state(now) != CircuitState::Open)
        .filter_map(|b| {
            b.serves(model).map(|m| Cand {
                backend: b.clone(),
                served_id: m.id,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        let ka = key(&a.backend);
        let kb = key(&b.backend);
        ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// (in-flight, latency ms, negative tok/s): smaller is better. Unknown latency counts as 0 so a
/// backend that has never been measured still gets traffic and builds a number.
fn key(b: &Backend) -> (usize, f64, f64) {
    (
        b.inflight(),
        b.ewma_ms().unwrap_or(0.0),
        -b.last_tok_s().unwrap_or(0.0),
    )
}

/// Ordered model ids to try for a requested name.
/// - `auto`: the single best model across healthy chat backends (fastest last tok/s, then idlest).
/// - an alias name: its list.
/// - any other id: itself, then the other members of every alias that contains it (declared fallbacks).
pub fn expand(pool: &Pool, ep: Endpoint, requested: &str) -> Vec<String> {
    if requested == "auto" {
        return best_auto(pool, ep).into_iter().collect();
    }
    if let Some(a) = pool.cfg.aliases.iter().find(|a| a.name == requested) {
        return a.models.clone();
    }
    let mut out = vec![requested.to_owned()];
    for a in &pool.cfg.aliases {
        if a.models.iter().any(|m| m == requested) {
            for m in &a.models {
                if !out.contains(m) {
                    out.push(m.clone());
                }
            }
        }
    }
    out
}

fn best_auto(pool: &Pool, ep: Endpoint) -> Option<String> {
    let now = Instant::now();
    pool.backends
        .iter()
        .filter(|b| ep.allows(b.role()) && b.is_up() && b.breaker.state(now) != CircuitState::Open)
        .flat_map(|b| b.model_ids().into_iter().map(move |m| (b.clone(), m)))
        .min_by(|(a, _), (b, _)| {
            let ta = a.last_tok_s().unwrap_or(-1.0);
            let tb = b.last_tok_s().unwrap_or(-1.0);
            tb.partial_cmp(&ta)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.inflight().cmp(&b.inflight()))
        })
        .map(|(_, m)| m)
}

/// True when any backend, in any state, or any configured hint knows this model id.
pub fn known_anywhere(pool: &Pool, model: &str) -> bool {
    pool.backends
        .iter()
        .any(|b| b.serves(model).is_some() || b.cfg.models.iter().any(|m| m == model))
}
