//! HTTP surface: OpenAI-compatible routes, `/pool/status`, `/health`, `/metrics`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use serde_json::{Value, json};

use crate::breaker::CircuitState;
use crate::config::Role;
use crate::proxy::{handle, oai_error};
use crate::route::Endpoint;
use crate::state::{Backend, Device, HealthState, Pool, lock, read};
use crate::util::now_iso;

pub fn router(pool: Arc<Pool>) -> Router {
    let limit = pool.cfg.server.max_body_mib * 1024 * 1024;
    let mut r = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/models", get(models))
        .route("/pool/status", get(status))
        .route("/health", get(health))
        .route("/", get(crate::ui::page))
        .route("/status", get(crate::ui::page))
        .route("/status.css", get(crate::ui::css))
        .route("/status.js", get(crate::ui::js))
        .route("/theme.css", get(crate::ui::theme));
    if pool.cfg.server.metrics {
        r = r.route("/metrics", get(metrics));
    }
    r.fallback(not_found)
        .layer(DefaultBodyLimit::max(limit))
        .layer(middleware::from_fn_with_state(pool.clone(), guard))
        .with_state(pool)
}

/// DNS-rebinding and CSRF guard, same shape as Fleet Studio's: loopback Host only, cross-origin
/// POSTs refused, POST bodies must be JSON. Also counts responses for `/metrics`.
async fn guard(State(pool): State<Arc<Pool>>, req: Request, next: Next) -> Response {
    let port = pool.cfg.bind_addr().map(|a| a.port()).unwrap_or(0);
    let allowed = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    let hdr = |h: &HeaderMap, n: header::HeaderName| {
        h.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned)
    };
    let path = match req.uri().path() {
        p @ ("/v1/chat/completions"
        | "/v1/completions"
        | "/v1/embeddings"
        | "/v1/models"
        | "/pool/status"
        | "/health"
        | "/status"
        | "/status.css"
        | "/status.js"
        | "/theme.css"
        | "/metrics") => p.to_owned(),
        _ => "other".to_owned(),
    };
    let denied = if !hdr(req.headers(), header::HOST).is_some_and(|h| allowed.contains(&h)) {
        Some(oai_error(
            StatusCode::FORBIDDEN,
            "forbidden host",
            "forbidden_host",
        ))
    } else if req.method() == axum::http::Method::POST {
        let origin_bad = hdr(req.headers(), header::ORIGIN)
            .filter(|o| !o.is_empty())
            .is_some_and(|o| !allowed.iter().any(|a| o == format!("http://{a}")));
        let json_ct = hdr(req.headers(), header::CONTENT_TYPE)
            .is_some_and(|c| c.contains("application/json"));
        if origin_bad {
            Some(oai_error(
                StatusCode::FORBIDDEN,
                "forbidden origin",
                "forbidden_origin",
            ))
        } else if !json_ct {
            Some(oai_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "content-type must be application/json",
                "unsupported_media_type",
            ))
        } else {
            None
        }
    } else {
        None
    };
    let resp = match denied {
        Some(r) => r,
        None => next.run(req).await,
    };
    pool.count_http(&path, resp.status().as_u16());
    resp
}

async fn not_found() -> Response {
    oai_error(StatusCode::NOT_FOUND, "unknown route", "not_found")
}

async fn chat(State(pool): State<Arc<Pool>>, body: Bytes) -> Response {
    handle(&pool, Endpoint::Chat, body).await
}

async fn completions(State(pool): State<Arc<Pool>>, body: Bytes) -> Response {
    handle(&pool, Endpoint::Completions, body).await
}

async fn embeddings(State(pool): State<Arc<Pool>>, body: Bytes) -> Response {
    handle(&pool, Endpoint::Embeddings, body).await
}

// ---- /health ----

async fn health(State(pool): State<Arc<Pool>>) -> Response {
    let chat_up = pool
        .backends
        .iter()
        .filter(|b| matches!(b.role(), Role::Chat | Role::Slot) && b.is_up())
        .count();
    let devices_up = pool
        .devices
        .iter()
        .enumerate()
        .filter(|(i, _)| device_state(&pool, *i).0 == "up")
        .count();
    let (code, status) = match (chat_up, devices_up) {
        (0, _) => (StatusCode::SERVICE_UNAVAILABLE, "down"),
        (_, n) if n < pool.devices.len() => (StatusCode::OK, "degraded"),
        _ => (StatusCode::OK, "ok"),
    };
    (
        code,
        axum::Json(json!({
            "ok": code == StatusCode::OK,
            "status": status,
            "service": "vram-pool",
            "chat_backends_up": chat_up,
            "devices_up": devices_up,
            "devices_total": pool.devices.len(),
        })),
    )
        .into_response()
}

// ---- /v1/models ----

#[derive(Default, serde::Deserialize)]
struct ModelsQuery {
    all: Option<String>,
}

fn truthy(v: &Option<String>) -> bool {
    matches!(v.as_deref(), Some("1" | "true" | "yes" | ""))
}

async fn models(State(pool): State<Arc<Pool>>, Query(q): Query<ModelsQuery>) -> Response {
    axum::Json(models_json(&pool, truthy(&q.all)).await).into_response()
}

struct Place {
    device: String,
    backend: String,
    residency: &'static str,
    role: &'static str,
    ctx: Option<u64>,
    size_bytes: Option<u64>,
    available: bool,
}

/// Union of models across devices. By default only models a healthy backend serves right now.
/// `?all=1` adds models of down backends (last known, or from config) and library files the 4080
/// slot could hot-swap in, each marked `available: false`.
pub async fn models_json(pool: &Arc<Pool>, all: bool) -> Value {
    let mut flat: Vec<(String, Place)> = Vec::new();
    for b in &pool.backends {
        let dev = pool.device_of(b).cfg.id.clone();
        let up = b.is_up();
        if !up && !all {
            continue;
        }
        let residency = if !up {
            "down"
        } else if b.role() == Role::Slot {
            "slot"
        } else {
            "resident"
        };
        let mut seen: Vec<String> = Vec::new();
        for m in read(&b.models).iter() {
            seen.push(m.id.clone());
            flat.push((
                m.id.clone(),
                Place {
                    device: dev.clone(),
                    backend: b.id().to_owned(),
                    residency,
                    role: b.role().as_str(),
                    ctx: m.ctx,
                    size_bytes: m.size_bytes,
                    available: up,
                },
            ));
        }
        if !up {
            for id in &b.cfg.models {
                if !seen.contains(id) {
                    flat.push((
                        id.clone(),
                        Place {
                            device: dev.clone(),
                            backend: b.id().to_owned(),
                            residency,
                            role: b.role().as_str(),
                            ctx: b.cfg.ctx,
                            size_bytes: None,
                            available: false,
                        },
                    ));
                }
            }
        }
    }
    if all
        && pool.swap.cfg.enabled
        && let Some(fleet) = pool.fleet.as_ref()
        && let Some(slot) = pool
            .backends
            .iter()
            .find(|b| b.role() == Role::Slot && b.cfg.fleet_host.is_some())
        && let Ok(lib) = fleet.library(crate::fleet::SWAPPABLE_HOST).await
    {
        let dev = pool.device_of(slot).cfg.id.clone();
        for e in lib {
            let stem = e.name.trim_end_matches(".gguf").to_owned();
            let want = crate::util::norm_model(&stem);
            if !flat
                .iter()
                .any(|(id, _)| crate::util::norm_model(id) == want)
            {
                flat.push((
                    stem,
                    Place {
                        device: dev.clone(),
                        backend: slot.id().to_owned(),
                        residency: "library",
                        role: "slot",
                        ctx: Some(u64::from(pool.swap.cfg.ctx)),
                        size_bytes: Some(e.size_bytes),
                        available: false,
                    },
                ));
            }
        }
    }
    let mut order: Vec<String> = Vec::new();
    let mut by_id: BTreeMap<String, Vec<Place>> = BTreeMap::new();
    for (id, p) in flat {
        if !by_id.contains_key(&id) {
            order.push(id.clone());
        }
        by_id.entry(id).or_default().push(p);
    }

    let started = pool.started.elapsed().as_secs();
    let created = chrono::Utc::now()
        .timestamp()
        .saturating_sub(started as i64);
    let data: Vec<Value> = order
        .iter()
        .filter_map(|id| {
            let places = by_id.get(id)?;
            let best = places.iter().find(|p| p.available).unwrap_or(&places[0]);
            let routable = places.iter().any(|p| matches!(p.role, "chat" | "slot" | "embedding"));
            let mut devices: Vec<&str> = Vec::new();
            for p in places {
                if !devices.contains(&p.device.as_str()) {
                    devices.push(&p.device);
                }
            }
            Some(json!({
                "id": id,
                "object": "model",
                "created": created,
                "owned_by": best.device,
                "device": best.device,
                "devices": devices,
                "backend": best.backend,
                "residency": best.residency,
                "role": best.role,
                "ctx": best.ctx,
                "available": places.iter().any(|p| p.available),
                "routable": routable,
                "placements": places.iter().map(|p| json!({
                    "device": p.device, "backend": p.backend, "residency": p.residency,
                    "role": p.role, "ctx": p.ctx, "size_bytes": p.size_bytes, "available": p.available,
                })).collect::<Vec<_>>(),
            }))
        })
        .collect();
    let mut aliases: Vec<Value> = pool
        .cfg
        .aliases
        .iter()
        .map(|a| json!({ "name": a.name, "models": a.models }))
        .collect();
    aliases.sort_by_key(|v| v["name"].as_str().unwrap_or_default().to_owned());
    json!({ "object": "list", "data": data, "aliases": aliases })
}

// ---- /pool/status ----

/// (state, reason). `up` when any backend is up, `idle` when only an empty slot answers, else `down`
/// with the backend reasons joined.
fn device_state(pool: &Pool, idx: usize) -> (&'static str, Option<String>) {
    let mut any_up = false;
    let mut any_idle = false;
    let mut reasons: Vec<String> = Vec::new();
    for b in pool.backends_of(idx) {
        let h = read(&b.health);
        match h.state {
            HealthState::Up => any_up = true,
            HealthState::Idle => any_idle = true,
            HealthState::Down => reasons.push(format!(
                "{}: {}",
                b.id(),
                h.reason.clone().unwrap_or_else(|| "down".to_owned())
            )),
        }
    }
    if any_up {
        ("up", None)
    } else if any_idle {
        ("idle", None)
    } else if reasons.is_empty() {
        (
            "down",
            Some("no backends configured for this device".to_owned()),
        )
    } else {
        ("down", Some(reasons.join("; ")))
    }
}

fn backend_json(pool: &Pool, b: &Backend) -> Value {
    let now = Instant::now();
    let (rps, tok, ok, failed, by_status, ewma) = {
        let mut s = lock(&b.stats);
        let rps = s.rps(now, pool.started.elapsed());
        (
            rps,
            s.last_tok_s,
            s.ok,
            s.failed,
            s.by_status.clone(),
            s.ewma_ms,
        )
    };
    let h = read(&b.health);
    let models: Vec<Value> = read(&b.models)
        .iter()
        .map(|m| json!({ "id": m.id, "ctx": m.ctx, "size_bytes": m.size_bytes, "residency": if b.role() == Role::Slot { "slot" } else { "resident" } }))
        .collect();
    json!({
        "id": b.id(),
        "url": b.cfg.url,
        "role": b.role().as_str(),
        "engine": b.cfg.engine,
        "state": h.state.as_str(),
        "reason": h.reason,
        "latency_ms": h.latency_ms,
        "checked_at": h.checked_at,
        "since": h.since,
        "circuit": b.breaker.state(now).as_str(),
        "circuit_opened_total": b.breaker.opened_total(),
        "inflight": b.inflight(),
        "requests_per_s": round2(rps),
        "last_tok_s": tok,
        "avg_response_ms": ewma.map(|v| v.round()),
        "requests_ok": ok,
        "requests_failed": failed,
        "responses_by_status": by_status.iter().map(|(k, v)| (k.to_string(), *v)).collect::<BTreeMap<_, _>>(),
        "models": models,
        "note": b.cfg.note,
        "routable": Endpoint::Chat.allows(b.role()) || b.role() == Role::Embedding,
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn device_json(pool: &Pool, idx: usize, d: &Device) -> Value {
    let (state, reason) = device_state(pool, idx);
    let backends: Vec<&Arc<Backend>> = pool.backends_of(idx).collect();
    let mem = read(&d.memory).clone();
    let now = Instant::now();
    let mut roles: Vec<&str> = backends.iter().map(|b| b.role().as_str()).collect();
    roles.sort_unstable();
    roles.dedup();
    let role = if backends
        .iter()
        .any(|b| matches!(b.role(), Role::Chat | Role::Slot))
    {
        "chat"
    } else if backends.iter().any(|b| b.role() == Role::Embedding) {
        "embedding"
    } else {
        "decision"
    };
    let resident: Vec<String> = backends
        .iter()
        .filter(|b| b.is_up())
        .flat_map(|b| b.model_ids())
        .collect();
    let mut rps = 0.0;
    let mut inflight = 0usize;
    let mut tok: Option<(Instant, f64)> = None;
    for b in &backends {
        let mut s = lock(&b.stats);
        rps += s.rps(now, pool.started.elapsed());
        inflight += b.inflight();
        if let (Some(t), Some(at)) = (s.last_tok_s, s.last_tok_at)
            && tok.is_none_or(|(prev, _)| at > prev)
        {
            tok = Some((at, t));
        }
    }
    json!({
        "id": d.cfg.id,
        "label": d.cfg.label,
        "kind": d.cfg.kind.as_str(),
        "state": state,
        "reason": reason,
        "role": role,
        "roles": roles,
        "memory": {
            "source": d.cfg.memory.source.as_str(),
            "total_mib": mem.total_mib,
            "used_mib": mem.used_mib,
            "free_mib": mem.free_mib,
            "ok": mem.ok,
            "error": mem.error,
            "checked_at": mem.checked_at,
            "load1": mem.load1,
            "note": d.cfg.memory.note,
            "apps": mem.apps.iter().map(|a| json!({ "pid": a.pid, "name": a.name, "used_mib": a.used_mib })).collect::<Vec<_>>(),
        },
        "models_resident": resident,
        "inflight": inflight,
        "requests_per_s": round2(rps),
        "last_tok_s": tok.map(|(_, t)| t),
        "backends": backends.iter().map(|b| backend_json(pool, b)).collect::<Vec<_>>(),
    })
}

pub fn status_json(pool: &Pool) -> Value {
    let mut devices = Vec::new();
    let (mut total, mut used, mut free, mut configured) = (0u64, 0u64, 0u64, 0u64);
    let (mut up, mut unreported) = (0usize, Vec::<String>::new());
    for (i, d) in pool.devices.iter().enumerate() {
        let v = device_json(pool, i, d);
        let state = device_state(pool, i).0;
        let mem = read(&d.memory);
        if let Some(t) = mem.total_mib {
            configured += t;
        }
        if state == "up" {
            up += 1;
        }
        if state != "down" {
            match (mem.total_mib, mem.used_mib, mem.free_mib) {
                (Some(t), Some(u), Some(f)) => {
                    total += t;
                    used += u;
                    free += f;
                }
                _ => unreported.push(d.cfg.id.clone()),
            }
        }
        devices.push(v);
    }
    let last_swap = lock(&pool.swap.last).clone().map(|r| {
        json!({ "at": r.at, "model": r.model, "file": r.file, "result": r.result, "detail": r.detail, "ms": r.ms })
    });
    let slot = pool
        .backends
        .iter()
        .find(|b| b.role() == Role::Slot && b.cfg.fleet_host.is_some())
        .map(|b| b.id().to_owned());
    json!({
        "service": "vram-pool",
        "version": env!("CARGO_PKG_VERSION"),
        "generated_at": now_iso(),
        "uptime_s": pool.started.elapsed().as_secs(),
        "pooled": {
            "devices_total": pool.devices.len(),
            "devices_up": up,
            "total_mib": total,
            "used_mib": used,
            "free_mib": free,
            "configured_total_mib": configured,
            "unreported_devices": unreported,
            "note": "total/used/free sum devices that are not down and report all three numbers; configured_total_mib also counts down devices with a known size",
        },
        "devices": devices,
        "swap": {
            "enabled": pool.swap.cfg.enabled && pool.fleet.is_some(),
            "slot_backend": slot,
            "requested": pool.swap.requested.load(std::sync::atomic::Ordering::Relaxed),
            "succeeded": pool.swap.succeeded.load(std::sync::atomic::Ordering::Relaxed),
            "failed": pool.swap.failed.load(std::sync::atomic::Ordering::Relaxed),
            "last": last_swap,
        },
    })
}

async fn status(State(pool): State<Arc<Pool>>) -> Response {
    axum::Json(status_json(&pool)).into_response()
}

// ---- /metrics ----

async fn metrics(State(pool): State<Arc<Pool>>) -> Response {
    let mut o = String::new();
    let now = Instant::now();
    o.push_str("# TYPE vram_pool_uptime_seconds gauge\n");
    o.push_str(&format!(
        "vram_pool_uptime_seconds {}\n",
        pool.started.elapsed().as_secs()
    ));
    o.push_str("# TYPE vram_pool_device_up gauge\n# TYPE vram_pool_device_memory_mib gauge\n");
    for (i, d) in pool.devices.iter().enumerate() {
        let id = &d.cfg.id;
        o.push_str(&format!(
            "vram_pool_device_up{{device=\"{id}\"}} {}\n",
            u8::from(device_state(&pool, i).0 == "up")
        ));
        let m = read(&d.memory);
        for (kind, v) in [
            ("total", m.total_mib),
            ("used", m.used_mib),
            ("free", m.free_mib),
        ] {
            if let Some(v) = v {
                o.push_str(&format!(
                    "vram_pool_device_memory_mib{{device=\"{id}\",kind=\"{kind}\"}} {v}\n"
                ));
            }
        }
    }
    o.push_str("# TYPE vram_pool_backend_up gauge\n# TYPE vram_pool_backend_circuit_open gauge\n# TYPE vram_pool_backend_inflight gauge\n");
    o.push_str("# TYPE vram_pool_backend_requests_total counter\n# TYPE vram_pool_backend_failures_total counter\n");
    for b in &pool.backends {
        let id = b.id();
        let dev = &pool.device_of(b).cfg.id;
        o.push_str(&format!(
            "vram_pool_backend_up{{backend=\"{id}\",device=\"{dev}\"}} {}\n",
            u8::from(b.is_up())
        ));
        o.push_str(&format!(
            "vram_pool_backend_circuit_open{{backend=\"{id}\"}} {}\n",
            u8::from(b.breaker.state(now) == CircuitState::Open)
        ));
        o.push_str(&format!(
            "vram_pool_backend_inflight{{backend=\"{id}\"}} {}\n",
            b.inflight()
        ));
        let s = lock(&b.stats);
        for (code, n) in &s.by_status {
            o.push_str(&format!(
                "vram_pool_backend_requests_total{{backend=\"{id}\",status=\"{code}\"}} {n}\n"
            ));
        }
        o.push_str(&format!(
            "vram_pool_backend_failures_total{{backend=\"{id}\"}} {}\n",
            s.failed
        ));
        if let Some(t) = s.last_tok_s {
            o.push_str(&format!(
                "vram_pool_backend_last_tokens_per_second{{backend=\"{id}\"}} {t}\n"
            ));
        }
    }
    o.push_str("# TYPE vram_pool_swap_total counter\n");
    for (r, v) in [
        ("requested", &pool.swap.requested),
        ("succeeded", &pool.swap.succeeded),
        ("failed", &pool.swap.failed),
    ] {
        o.push_str(&format!(
            "vram_pool_swap_total{{result=\"{r}\"}} {}\n",
            v.load(std::sync::atomic::Ordering::Relaxed)
        ));
    }
    o.push_str("# TYPE vram_pool_http_requests_total counter\n");
    for ((path, code), n) in lock(&pool.http_requests).iter() {
        o.push_str(&format!(
            "vram_pool_http_requests_total{{path=\"{path}\",code=\"{code}\"}} {n}\n"
        ));
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], o).into_response()
}
