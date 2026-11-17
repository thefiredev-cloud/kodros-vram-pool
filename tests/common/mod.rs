//! Fake backends, a fake Fleet Studio and a pool harness for integration tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bytes::Bytes;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use vram_pool::config::Config;
use vram_pool::probe;
use vram_pool::state::Pool;

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A llama.cpp-shaped fake: /health, /v1/models, chat (plain and SSE), embeddings.
#[derive(Clone)]
pub struct Fake {
    pub port: u16,
    pub name: String,
    pub models: Arc<RwLock<Vec<String>>>,
    pub hits: Arc<AtomicUsize>,
    pub chat_hits: Arc<AtomicUsize>,
    pub bodies: Arc<Mutex<Vec<Value>>>,
    /// Non-zero: chat answers with this HTTP status.
    pub chat_status: Arc<AtomicU16>,
    /// Pause between SSE chunks.
    pub chunk_delay_ms: Arc<AtomicU64>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Fake {
    pub async fn start(name: &str, models: &[&str]) -> Fake {
        Self::start_on(free_port(), name, models).await
    }

    pub async fn start_on(port: u16, name: &str, models: &[&str]) -> Fake {
        let f = Fake {
            port,
            name: name.to_owned(),
            models: Arc::new(RwLock::new(
                models.iter().map(|m| (*m).to_owned()).collect(),
            )),
            hits: Arc::default(),
            chat_hits: Arc::default(),
            bodies: Arc::default(),
            chat_status: Arc::default(),
            chunk_delay_ms: Arc::default(),
            handle: Arc::default(),
        };
        let app = Router::new()
            .route(
                "/health",
                get(|State(f): State<Fake>| async move {
                    f.hits.fetch_add(1, Ordering::Relaxed);
                    axum::Json(json!({"status": "ok"}))
                }),
            )
            .route("/v1/models", get(list))
            .route("/v1/chat/completions", post(chat))
            .route("/v1/completions", post(chat))
            .route("/v1/embeddings", post(embed))
            .layer(axum::middleware::map_response(
                |mut r: Response| async move {
                    // No keep-alive: once the task is aborted, new connections are refused for real.
                    r.headers_mut()
                        .insert(header::CONNECTION, HeaderValue::from_static("close"));
                    r
                },
            ))
            .with_state(f.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        let h = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        *f.handle.lock().unwrap() = Some(h);
        f
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Stops listening. Later connections are refused.
    pub fn stop(&self) {
        if let Some(h) = self.handle.lock().unwrap().take() {
            h.abort();
        }
    }

    pub fn chat_hits(&self) -> usize {
        self.chat_hits.load(Ordering::Relaxed)
    }

    pub fn last_body(&self) -> Option<Value> {
        self.bodies.lock().unwrap().last().cloned()
    }
}

async fn list(State(f): State<Fake>) -> Response {
    let data: Vec<Value> = f
        .models
        .read()
        .unwrap()
        .iter()
        .map(|m| json!({"id": m, "object": "model", "meta": {"n_ctx": 4096, "size": 1_000_000_000u64}}))
        .collect();
    axum::Json(json!({"object": "list", "data": data})).into_response()
}

async fn chat(State(f): State<Fake>, body: Bytes) -> Response {
    f.hits.fetch_add(1, Ordering::Relaxed);
    f.chat_hits.fetch_add(1, Ordering::Relaxed);
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    f.bodies.lock().unwrap().push(v.clone());
    let st = f.chat_status.load(Ordering::Relaxed);
    if st != 0 {
        return (StatusCode::from_u16(st).unwrap(), "boom").into_response();
    }
    if v.get("stream").and_then(Value::as_bool) == Some(true) {
        let delay = Duration::from_millis(f.chunk_delay_ms.load(Ordering::Relaxed));
        let chunks = vec![
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"hello from {}\"}}}}]}}\n\n",
                f.name
            ),
            "data: {\"choices\":[{\"delta\":{\"content\":\" more\"}}]}\n\n".to_owned(),
            "data: {\"timings\":{\"predicted_per_second\":88.8}}\n\n".to_owned(),
            "data: [DONE]\n\n".to_owned(),
        ];
        let s = futures_util::stream::unfold(chunks.into_iter(), move |mut it| async move {
            let c = it.next()?;
            tokio::time::sleep(delay).await;
            Some((Ok::<Bytes, std::io::Error>(Bytes::from(c)), it))
        });
        let mut r = Response::new(Body::from_stream(s));
        r.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        return r;
    }
    axum::Json(json!({
        "model": v.get("model"),
        "choices": [{"message": {"role": "assistant", "content": format!("hello from {}", f.name)}}],
        "usage": {"completion_tokens": 5},
        "timings": {"predicted_per_second": 123.4},
    }))
    .into_response()
}

async fn embed(State(f): State<Fake>, body: Bytes) -> Response {
    f.hits.fetch_add(1, Ordering::Relaxed);
    f.bodies
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
    axum::Json(json!({"data": [{"embedding": [0.1, 0.2, 0.3]}], "from": f.name})).into_response()
}

/// Fake Fleet Studio. `GET /api/status` lists a 4080 library and slot; `POST .../slot/swap` starts
/// the slot fake on its configured port with the swapped model.
#[derive(Clone)]
pub struct FakeFleet {
    pub port: u16,
    pub slot_port: u16,
    pub library: Arc<Vec<(String, u64)>>,
    pub swaps: Arc<Mutex<Vec<Value>>>,
    pub slot: Arc<Mutex<Option<Fake>>>,
    pub swap_status: Arc<AtomicU16>,
    pub swap_delay_ms: Arc<AtomicU64>,
    pub cluster_error: Arc<Mutex<Option<String>>>,
    handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl FakeFleet {
    pub async fn start(slot_port: u16, library: &[(&str, u64)]) -> FakeFleet {
        let f = FakeFleet {
            port: free_port(),
            slot_port,
            library: Arc::new(library.iter().map(|(n, s)| ((*n).to_owned(), *s)).collect()),
            swaps: Arc::default(),
            slot: Arc::default(),
            swap_status: Arc::default(),
            swap_delay_ms: Arc::default(),
            cluster_error: Arc::default(),
            handle: Arc::default(),
        };
        let app = Router::new()
            .route("/api/status", get(fleet_status))
            .route("/api/hosts/{host}/slot/swap", post(fleet_swap))
            .with_state(f.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", f.port))
            .await
            .unwrap();
        *f.handle.lock().unwrap() = Some(tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        }));
        f
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn swap_count(&self) -> usize {
        self.swaps.lock().unwrap().len()
    }

    pub fn stop_all(&self) {
        if let Some(h) = self.handle.lock().unwrap().take() {
            h.abort();
        }
        if let Some(s) = self.slot.lock().unwrap().take() {
            s.stop();
        }
    }
}

async fn fleet_status(State(f): State<FakeFleet>) -> Response {
    let lib: Vec<Value> = f
        .library
        .iter()
        .map(|(n, s)| json!({"name": n, "size": s}))
        .collect();
    let running = f.slot.lock().unwrap().is_some();
    let cluster_error = f.cluster_error.lock().unwrap().clone();
    axum::Json(json!({
        "hosts": [
            {"id": "rtx4080", "library": lib, "slot": {
                "state": if running { "running" } else { "stopped" },
                "url": format!("http://127.0.0.1:{}", f.slot_port), "swap": null}},
            {"id": "cluster", "error": cluster_error, "summary": "1/2 nodes up",
             "nodes": [{"id": "node-b", "label": "Node B", "online": false, "error": "ssh: connect timed out"}]},
        ]
    }))
    .into_response()
}

async fn fleet_swap(
    State(f): State<FakeFleet>,
    axum::extract::Path(host): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    assert_eq!(host, "rtx4080", "pool must only swap on rtx4080");
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains("application/json"))
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    f.swaps.lock().unwrap().push(v.clone());
    tokio::time::sleep(Duration::from_millis(
        f.swap_delay_ms.load(Ordering::Relaxed),
    ))
    .await;
    let st = f.swap_status.load(Ordering::Relaxed);
    if st != 0 {
        return (
            StatusCode::from_u16(st).unwrap(),
            axum::Json(
                json!({"ok": false, "error": "preflight", "reason": "refused: fake says no"}),
            ),
        )
            .into_response();
    }
    let file = v["model"].as_str().unwrap().to_owned();
    let id = format!("slot-4080/{}", file.trim_end_matches(".gguf"));
    if let Some(old) = f.slot.lock().unwrap().take() {
        old.stop();
    }
    let fake = Fake::start_on(f.slot_port, "slot", &[&id]).await;
    *f.slot.lock().unwrap() = Some(fake);
    axum::Json(json!({"ok": true, "record": {"to": file}})).into_response()
}

pub struct Harness {
    pub base: String,
    pub pool: Arc<Pool>,
    pub client: reqwest::Client,
    pub port: u16,
}

pub async fn start_pool(toml_text: &str) -> Harness {
    let cfg = Config::parse(toml_text).expect("test config parses");
    let port = cfg.bind_addr().unwrap().port();
    let pool = Pool::new(cfg).unwrap();
    probe::probe_all_once(&pool).await;
    probe::spawn_loops(&pool);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let p = pool.clone();
    tokio::spawn(async move {
        let _ = vram_pool::serve(p, listener, std::future::pending()).await;
    });
    Harness {
        base: format!("http://127.0.0.1:{port}"),
        pool,
        client: reqwest::Client::new(),
        port,
    }
}

impl Harness {
    pub async fn get_json(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    pub async fn chat(&self, model: &str) -> reqwest::Response {
        self.chat_body(json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
            .await
    }

    pub async fn chat_body(&self, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}/v1/chat/completions", self.base))
            .json(&body)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .unwrap()
    }

    /// Waits until `/pool/status` shows `backend` in `state` (or panics after `secs`).
    pub async fn wait_backend_state(&self, backend: &str, state: &str, secs: u64) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let st = self.get_json("/pool/status").await;
            if let Some(b) = find_backend(&st, backend)
                && b["state"] == state
            {
                return b;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "backend {backend} never reached {state}: {st:#}"
            );
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }
}

pub fn find_backend(status: &Value, backend: &str) -> Option<Value> {
    status["devices"]
        .as_array()?
        .iter()
        .flat_map(|d| d["backends"].as_array().cloned().unwrap_or_default())
        .find(|b| b["id"] == backend)
}

pub fn find_device(status: &Value, id: &str) -> Option<Value> {
    status["devices"]
        .as_array()?
        .iter()
        .find(|d| d["id"] == id)
        .cloned()
}

pub fn hdr(r: &reqwest::Response, name: &str) -> String {
    r.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}
