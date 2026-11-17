//! Request path: pick a backend, forward the body, stream the answer back.
//!
//! Failover only happens before the first response byte. Each backend is tried at most once per
//! request and at most `max_attempts` backends are tried in total, so a dead pool answers fast.

use std::collections::HashSet;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::route::{Cand, Endpoint, expand, known_anywhere, ranked};
use crate::state::{Backend, HealthState, InflightGuard, Pool, lock, read};
use crate::swap::{SwapOutcome, try_swap};
use crate::util::{describe_http_error, one_line};

pub fn oai_error(status: StatusCode, message: &str, code: &str) -> Response {
    let kind = if status.is_client_error() {
        "invalid_request_error"
    } else {
        "server_error"
    };
    (
        status,
        axum::Json(json!({ "error": { "message": message, "type": kind, "code": code } })),
    )
        .into_response()
}

pub async fn handle(pool: &Arc<Pool>, ep: Endpoint, body: Bytes) -> Response {
    let mut parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return oai_error(
                StatusCode::BAD_REQUEST,
                &format!("body is not valid JSON: {e}"),
                "invalid_json",
            );
        }
    };
    let Some(obj) = parsed.as_object() else {
        return oai_error(
            StatusCode::BAD_REQUEST,
            "body must be a JSON object",
            "invalid_json",
        );
    };
    let requested = obj
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .unwrap_or("auto")
        .to_owned();
    let streaming = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);

    let models = expand(pool, ep, &requested);
    let Some(primary) = models.first().cloned() else {
        return oai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no chat backend is up, so model \"auto\" has nothing to pick",
            "no_backend",
        );
    };

    let mut tried: HashSet<String> = HashSet::new();
    let mut order: Vec<Cand> = ranked(pool, ep, &primary, &tried);
    let mut notes: Vec<String> = Vec::new();
    // True when the model is a library file the slot could load but declined to (no room, cooldown).
    let mut swap_declined = false;
    if order.is_empty() {
        match try_swap(pool, &primary).await {
            SwapOutcome::Swapped | SwapOutcome::AlreadyResident => {
                order = ranked(pool, ep, &primary, &tried)
            }
            SwapOutcome::Declined(why) => {
                swap_declined = true;
                notes.push(format!("hot-swap: {why}"));
            }
            SwapOutcome::Skipped(why) => notes.push(format!("hot-swap: {why}")),
        }
    }
    for fallback in models.iter().skip(1) {
        for c in ranked(pool, ep, fallback, &tried) {
            if !order.iter().any(|o| o.backend.id() == c.backend.id()) {
                order.push(c);
            }
        }
    }

    if order.is_empty() {
        let known = swap_declined || models.iter().any(|m| known_anywhere(pool, m));
        let down = down_report(pool);
        let msg = format!(
            "no healthy backend serves {requested:?}. {}{}",
            notes.join("; "),
            if down.is_empty() {
                String::new()
            } else {
                format!(" Down: {down}")
            }
        );
        return if known {
            oai_error(StatusCode::SERVICE_UNAVAILABLE, msg.trim(), "no_backend")
        } else {
            oai_error(StatusCode::NOT_FOUND, msg.trim(), "model_not_found")
        };
    }

    let max_attempts = pool.cfg.server.max_attempts;
    let mut attempts = 0usize;
    let mut failures: Vec<String> = notes;
    for cand in order {
        if attempts >= max_attempts {
            break;
        }
        if tried.contains(cand.backend.id()) {
            continue;
        }
        if !cand.backend.breaker.allow(Instant::now()) {
            failures.push(format!("{}: circuit open", cand.backend.id()));
            continue;
        }
        tried.insert(cand.backend.id().to_owned());
        attempts += 1;
        // The body is forwarded untouched unless the backend serves the model under another id.
        let payload = if cand.served_id == requested {
            body.clone()
        } else {
            if let Some(o) = parsed.as_object_mut() {
                o.insert("model".to_owned(), Value::String(cand.served_id.clone()));
            }
            match serde_json::to_vec(&parsed) {
                Ok(v) => Bytes::from(v),
                Err(e) => {
                    return oai_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("re-encode body: {e}"),
                        "internal",
                    );
                }
            }
        };
        match attempt(pool, &cand, ep, payload, streaming, attempts).await {
            Ok(resp) => return resp,
            Err(why) => failures.push(format!("{}: {why}", cand.backend.id())),
        }
    }
    oai_error(
        StatusCode::SERVICE_UNAVAILABLE,
        &format!(
            "all candidate backends failed for {requested:?} after {attempts} attempt(s): {}",
            failures.join("; ")
        ),
        "backends_failed",
    )
}

/// Short list of down chat backends and why, for error messages.
fn down_report(pool: &Pool) -> String {
    let mut parts = Vec::new();
    for b in &pool.backends {
        if !Endpoint::Chat.allows(b.role()) {
            continue;
        }
        let h = read(&b.health);
        if h.state != HealthState::Up {
            parts.push(format!(
                "{} ({}): {}",
                b.id(),
                h.state.as_str(),
                one_line(h.reason.as_deref().unwrap_or("unknown"), 120)
            ));
        }
    }
    parts.join("; ")
}

async fn attempt(
    pool: &Arc<Pool>,
    cand: &Cand,
    ep: Endpoint,
    payload: Bytes,
    streaming: bool,
    attempt_no: usize,
) -> Result<Response, String> {
    let backend = cand.backend.clone();
    let guard = InflightGuard::new(backend.clone());
    lock(&backend.stats).note_request(Instant::now());
    let key = backend.key.get().await;
    let url = format!("{}{}", backend.cfg.url, ep.path());
    let mut req = pool
        .http
        .post(&url)
        .header(CONTENT_TYPE, "application/json")
        .body(payload);
    if let Some(k) = &key {
        req = req.bearer_auth(k);
    }
    let started = Instant::now();
    let header_timeout = Duration::from_secs(pool.cfg.server.request_timeout_s);
    let resp = match tokio::time::timeout(header_timeout, req.send()).await {
        Err(_) => {
            backend.breaker.failure(Instant::now());
            lock(&backend.stats).failed += 1;
            return Err(format!(
                "no response headers after {}s",
                header_timeout.as_secs()
            ));
        }
        Ok(Err(e)) => {
            backend.breaker.failure(Instant::now());
            lock(&backend.stats).failed += 1;
            return Err(describe_http_error(
                &e,
                Duration::from_secs(pool.cfg.server.connect_timeout_s),
            ));
        }
        Ok(Ok(r)) => r,
    };
    let status = resp.status();
    if status.is_server_error() && status != StatusCode::NOT_IMPLEMENTED {
        backend.breaker.failure(Instant::now());
        {
            let mut st = lock(&backend.stats);
            st.failed += 1;
            *st.by_status.entry(status.as_u16()).or_insert(0) += 1;
        }
        let snippet = tokio::time::timeout(Duration::from_secs(2), resp.text())
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let detail = if snippet.is_empty() {
            String::new()
        } else {
            format!(": {}", one_line(&snippet, 100))
        };
        return Err(format!("HTTP {}{detail}", status.as_u16()));
    }
    // Anything below 500 means the backend is alive. A 401 additionally drops the cached key.
    if matches!(status.as_u16(), 401 | 403) {
        backend.key.invalidate();
    }
    backend.breaker.success();
    {
        let mut st = lock(&backend.stats);
        st.note_latency(started.elapsed().as_secs_f64() * 1000.0);
        *st.by_status.entry(status.as_u16()).or_insert(0) += 1;
    }

    let mut headers = HeaderMap::new();
    for name in [CONTENT_TYPE, CACHE_CONTROL] {
        if let Some(v) = resp.headers().get(&name) {
            headers.insert(name, v.clone());
        }
    }
    let device_id = pool.device_of(&backend).cfg.id.clone();
    set_header(&mut headers, "x-vram-pool-device", &device_id);
    set_header(&mut headers, "x-vram-pool-backend", backend.id());
    set_header(&mut headers, "x-vram-pool-model", &cand.served_id);
    set_header(
        &mut headers,
        "x-vram-pool-attempts",
        &attempt_no.to_string(),
    );
    if streaming {
        headers.insert(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        );
    }

    let tap = Tap {
        backend: backend.clone(),
        started,
        tail: Vec::new(),
        finished: false,
    };
    let inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>> =
        Box::pin(resp.bytes_stream());
    let idle = Duration::from_secs(pool.cfg.server.stream_idle_timeout_s);
    let body_stream = futures_util::stream::unfold(
        (inner, tap, guard, false),
        move |(mut inner, mut tap, guard, done)| async move {
            if done {
                return None;
            }
            match tokio::time::timeout(idle, inner.next()).await {
                Ok(Some(Ok(chunk))) => {
                    tap.push(&chunk);
                    Some((
                        Ok::<Bytes, std::io::Error>(chunk),
                        (inner, tap, guard, false),
                    ))
                }
                Ok(Some(Err(e))) => {
                    tap.finish(false);
                    Some((
                        Err(std::io::Error::other(format!("upstream stream error: {e}"))),
                        (inner, tap, guard, true),
                    ))
                }
                Ok(None) => {
                    tap.finish(true);
                    None
                }
                Err(_) => {
                    tap.finish(false);
                    let err = std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("upstream silent for {}s", idle.as_secs()),
                    );
                    Some((Err(err), (inner, tap, guard, true)))
                }
            }
        },
    );
    let mut out = Response::new(Body::from_stream(body_stream));
    *out.status_mut() = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    *out.headers_mut() = headers;
    Ok(out)
}

fn set_header(h: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(v) = HeaderValue::from_str(value) {
        h.insert(HeaderName::from_static(name), v);
    }
}

const TAIL_CAP: usize = 8192;

/// Watches the byte stream without buffering it: keeps the last 8 KiB to read llama.cpp's
/// `timings` or the OpenAI `usage` block once the stream ends. A client that drops the stream
/// early leaves no mark, good or bad.
struct Tap {
    backend: Arc<Backend>,
    started: Instant,
    tail: Vec<u8>,
    finished: bool,
}

impl Tap {
    fn push(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        if self.tail.len() > TAIL_CAP {
            let cut = self.tail.len() - TAIL_CAP;
            self.tail.drain(..cut);
        }
    }

    fn finish(&mut self, ok: bool) {
        if self.finished {
            return;
        }
        self.finished = true;
        if ok {
            let total_s = self.started.elapsed().as_secs_f64();
            let mut st = lock(&self.backend.stats);
            st.ok += 1;
            if let Some(t) = extract_tok_s(&self.tail, total_s) {
                st.last_tok_s = Some((t * 10.0).round() / 10.0);
                st.last_tok_at = Some(Instant::now());
            }
        } else {
            self.backend.breaker.failure(Instant::now());
            lock(&self.backend.stats).failed += 1;
        }
    }
}

/// llama.cpp reports `"predicted_per_second"`; OpenAI-style servers only `completion_tokens`,
/// which is divided by wall time (an underestimate, so it is only the fallback).
pub fn extract_tok_s(tail: &[u8], total_s: f64) -> Option<f64> {
    let text = String::from_utf8_lossy(tail);
    if let Some(v) = number_after(&text, "\"predicted_per_second\":")
        && v > 0.0
    {
        return Some(v);
    }
    let n = number_after(&text, "\"completion_tokens\":")?;
    (n > 0.0 && total_s > 0.05).then(|| n / total_s)
}

fn number_after(text: &str, key: &str) -> Option<f64> {
    let at = text.rfind(key)? + key.len();
    let rest = text[at..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+')))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tok_s_prefers_llamacpp_timings() {
        let tail = br#"data: {"usage":{"completion_tokens":64},"timings":{"predicted_per_second":251.7,"x":1}}"#;
        assert_eq!(extract_tok_s(tail, 1.0), Some(251.7));
    }

    #[test]
    fn tok_s_falls_back_to_usage_over_wall_time() {
        let tail = br#"{"usage":{"completion_tokens":40}}"#;
        assert_eq!(extract_tok_s(tail, 2.0), Some(20.0));
        assert_eq!(extract_tok_s(b"{}", 2.0), None);
    }
}
