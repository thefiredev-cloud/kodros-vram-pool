//! Integration tests: the real pool in front of fake backends and a fake Fleet Studio.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::{Fake, FakeFleet, Harness, find_backend, find_device, free_port, hdr, start_pool};
use futures_util::StreamExt;
use serde_json::{Value, json};
use vram_pool::fleet::{Fleet, SWAPPABLE_HOST};

fn server(port: u16, extra: &str) -> String {
    format!(
        r#"
[server]
bind = "127.0.0.1:{port}"
health_interval_s = 1
memory_interval_s = 1
probe_timeout_s = 1
connect_timeout_s = 1
request_timeout_s = 5
stream_idle_timeout_s = 5
breaker_failures = 2
breaker_open_s = 30
{extra}
"#
    )
}

fn device(id: &str, extra: &str) -> String {
    format!("\n[[device]]\nid = \"{id}\"\nkind = \"gpu\"\n{extra}\n")
}

fn backend(id: &str, device: &str, port: u16, role: &str, extra: &str) -> String {
    format!(
        "\n[[backend]]\nid = \"{id}\"\ndevice = \"{device}\"\nurl = \"http://127.0.0.1:{port}\"\nrole = \"{role}\"\n{extra}\n"
    )
}

async fn two_chat(a_models: &[&str], b_models: &[&str]) -> (Fake, Fake, Harness) {
    let a = Fake::start("alpha", a_models).await;
    let b = Fake::start("beta", b_models).await;
    let cfg = server(free_port(), "")
        + &device("dev-a", "")
        + &device("dev-b", "")
        + &backend("a", "dev-a", a.port, "chat", "")
        + &backend("b", "dev-b", b.port, "chat", "");
    let h = start_pool(&cfg).await;
    (a, b, h)
}

#[tokio::test(flavor = "multi_thread")]
async fn routes_to_resident_model_and_reports_device() {
    let (a, b, h) = two_chat(&["m-alpha"], &["m-beta"]).await;
    let r = h.chat("m-alpha").await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "x-vram-pool-backend"), "a");
    assert_eq!(hdr(&r, "x-vram-pool-device"), "dev-a");
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hello from alpha");
    let r = h.chat("m-beta").await;
    assert_eq!(hdr(&r, "x-vram-pool-backend"), "b");
    assert_eq!(a.chat_hits(), 1);
    assert_eq!(b.chat_hits(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn models_lists_union_with_device_residency_and_ctx() {
    let (_a, _b, h) = two_chat(&["m-alpha", "shared"], &["m-beta", "shared"]).await;
    let v = h.get_json("/v1/models").await;
    let data = v["data"].as_array().unwrap();
    let ids: Vec<&str> = data.iter().filter_map(|m| m["id"].as_str()).collect();
    assert_eq!(ids.len(), 3, "union is unique per id: {ids:?}");
    let alpha = data.iter().find(|m| m["id"] == "m-alpha").unwrap();
    assert_eq!(alpha["device"], "dev-a");
    assert_eq!(alpha["residency"], "resident");
    assert_eq!(alpha["ctx"], 4096);
    let shared = data.iter().find(|m| m["id"] == "shared").unwrap();
    assert_eq!(shared["devices"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn decision_models_are_listed_but_never_get_chat_traffic() {
    let chat = Fake::start("chat", &["chat-model"]).await;
    let judge = Fake::start("judge", &["judge-model"]).await;
    let cfg = server(free_port(), "")
        + &device("d", "")
        + &backend("chat", "d", chat.port, "chat", "")
        + &backend("judge", "d", judge.port, "decision", "");
    let h = start_pool(&cfg).await;
    let models = h.get_json("/v1/models").await;
    let j = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "judge-model")
        .unwrap();
    assert_eq!(j["role"], "decision");
    assert_eq!(j["routable"], false);
    let r = h.chat("judge-model").await;
    assert_eq!(r.status(), 503, "known model, no routable backend");
    assert_eq!(
        judge.chat_hits(),
        0,
        "decision backend must never receive chat"
    );
    // `auto` must not pick the decision model either.
    let r = h.chat("auto").await;
    assert_eq!(hdr(&r, "x-vram-pool-backend"), "chat");
    assert_eq!(judge.chat_hits(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn lowest_queue_wins_between_equal_backends() {
    let a = Fake::start("alpha", &["same"]).await;
    let b = Fake::start("beta", &["same"]).await;
    a.chunk_delay_ms.store(400, Ordering::Relaxed);
    b.chunk_delay_ms.store(400, Ordering::Relaxed);
    let cfg = server(free_port(), "")
        + &device("d1", "")
        + &device("d2", "")
        + &backend("a", "d1", a.port, "chat", "")
        + &backend("b", "d2", b.port, "chat", "");
    let h = start_pool(&cfg).await;
    // Hold one stream open on whichever backend wins first.
    let first = h
        .chat_body(json!({"model": "same", "stream": true, "messages": []}))
        .await;
    let busy = hdr(&first, "x-vram-pool-backend");
    let second = h.chat("same").await;
    let other = hdr(&second, "x-vram-pool-backend");
    assert_ne!(busy, other, "second request must avoid the busy backend");
    drop(first);
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_is_passed_through_incrementally() {
    let a = Fake::start("alpha", &["s"]).await;
    a.chunk_delay_ms.store(200, Ordering::Relaxed);
    let cfg = server(free_port(), "") + &device("d", "") + &backend("a", "d", a.port, "chat", "");
    let h = start_pool(&cfg).await;
    let t0 = Instant::now();
    let resp = h
        .chat_body(json!({"model": "s", "stream": true, "messages": []}))
        .await;
    assert_eq!(resp.status(), 200);
    assert!(hdr(&resp, "content-type").contains("text/event-stream"));
    let mut stream = resp.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    let t_first = t0.elapsed();
    assert!(String::from_utf8_lossy(&first).contains("hello from alpha"));
    let mut all = first.to_vec();
    while let Some(c) = stream.next().await {
        all.extend_from_slice(&c.unwrap());
    }
    let t_all = t0.elapsed();
    let text = String::from_utf8(all).unwrap();
    assert!(text.contains("data: [DONE]"), "{text}");
    assert!(
        t_first < Duration::from_millis(600),
        "first chunk after {t_first:?} means the pool buffered"
    );
    assert!(
        t_all >= Duration::from_millis(700),
        "full stream took {t_all:?}; the fake paces 4 chunks at 200 ms"
    );
    // The pool read llama.cpp's timings from the stream tail.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let st = h.get_json("/pool/status").await;
    assert_eq!(find_backend(&st, "a").unwrap()["last_tok_s"], 88.8);
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_backend_flips_down_within_interval_and_requests_fail_over() {
    let (a, b, h) = two_chat(&["same"], &["same"]).await;
    let up = h.get_json("/pool/status").await;
    assert_eq!(find_backend(&up, "a").unwrap()["state"], "up");
    a.stop();
    let t0 = Instant::now();
    let dead = h.wait_backend_state("a", "down", 4).await;
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "flip took {:?}, interval is 1 s",
        t0.elapsed()
    );
    assert!(
        dead["reason"].as_str().unwrap().contains("refused"),
        "{dead}"
    );
    let st = h.get_json("/pool/status").await;
    assert_eq!(find_device(&st, "dev-a").unwrap()["state"], "down");
    assert!(
        find_device(&st, "dev-a").unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("refused")
    );
    // Requests go to the survivor.
    for _ in 0..3 {
        let r = h.chat("same").await;
        assert_eq!(r.status(), 200);
        assert_eq!(hdr(&r, "x-vram-pool-backend"), "b");
    }
    assert_eq!(b.chat_hits(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn failover_happens_inside_one_request_when_backend_dies_between_probes() {
    // Health interval is long, so the pool still believes `a` is up when the request arrives.
    let a = Fake::start("alpha", &["same"]).await;
    let b = Fake::start("beta", &["same"]).await;
    let cfg = server(free_port(), "").replace("health_interval_s = 1", "health_interval_s = 600")
        + &device("d1", "")
        + &device("d2", "")
        + &backend("a", "d1", a.port, "chat", "")
        + &backend("b", "d2", b.port, "chat", "");
    let h = start_pool(&cfg).await;
    a.stop();
    // `a` and `b` tie at zero in-flight; make `a` the first pick by giving `b` a measured latency.
    vram_pool::state::lock(&h.pool.backends[1].stats).note_latency(500.0);
    let r = h.chat("same").await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "x-vram-pool-backend"), "b");
    assert_eq!(
        hdr(&r, "x-vram-pool-attempts"),
        "2",
        "first try hit the dead backend, second succeeded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_recovers_automatically_when_it_returns() {
    let a = Fake::start("alpha", &["m"]).await;
    let port = a.port;
    let cfg = server(free_port(), "") + &device("d", "") + &backend("a", "d", port, "chat", "");
    let h = start_pool(&cfg).await;
    a.stop();
    h.wait_backend_state("a", "down", 4).await;
    let r = h.chat("m").await;
    assert_eq!(
        r.status(),
        503,
        "the model is known (last seen on `a`) but nothing healthy serves it"
    );
    let _back = Fake::start_on(port, "alpha-again", &["m"]).await;
    h.wait_backend_state("a", "up", 5).await;
    let r = h.chat("m").await;
    assert_eq!(r.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn circuit_opens_after_failures_and_stops_hitting_the_backend() {
    let bad = Fake::start("bad", &["same"]).await;
    let good = Fake::start("good", &["same"]).await;
    bad.chat_status.store(500, Ordering::Relaxed);
    let cfg = server(free_port(), "")
        + &device("d1", "")
        + &device("d2", "")
        + &backend("bad", "d1", bad.port, "chat", "")
        + &backend("good", "d2", good.port, "chat", "");
    let h = start_pool(&cfg).await;
    // Force `bad` first so it takes the failures: give `good` a worse measured latency.
    vram_pool::state::lock(&h.pool.backends[1].stats).note_latency(900.0);
    for _ in 0..6 {
        let r = h.chat("same").await;
        assert_eq!(r.status(), 200, "failover must hide the broken backend");
    }
    let hits = bad.chat_hits();
    assert_eq!(
        hits, 2,
        "breaker threshold is 2: exactly 2 failures reach the backend, then the circuit opens (got {hits})"
    );
    let st = h.get_json("/pool/status").await;
    let b = find_backend(&st, "bad").unwrap();
    assert_eq!(b["circuit"], "open");
    assert_eq!(b["state"], "up", "health is fine; only the circuit is open");
    assert_eq!(b["circuit_opened_total"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_retry_storm_when_every_backend_fails() {
    let fakes = [
        Fake::start("x", &["same"]).await,
        Fake::start("y", &["same"]).await,
        Fake::start("z", &["same"]).await,
    ];
    for f in &fakes {
        f.chat_status.store(503, Ordering::Relaxed);
    }
    let mut cfg = server(free_port(), "").replace("breaker_failures = 2", "breaker_failures = 50");
    for (i, f) in fakes.iter().enumerate() {
        cfg += &device(&format!("d{i}"), "");
        cfg += &backend(&format!("b{i}"), &format!("d{i}"), f.port, "chat", "");
    }
    let h = start_pool(&cfg).await;
    let r = h.chat("same").await;
    assert_eq!(r.status(), 503);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "backends_failed");
    let total: usize = fakes.iter().map(Fake::chat_hits).sum();
    assert_eq!(total, 3, "one try per backend, no retries on the same one");
    assert!(fakes.iter().all(|f| f.chat_hits() == 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn alias_fallback_rewrites_model_id() {
    let dead = Fake::start("dead", &["phone-2b"]).await;
    let alive = Fake::start("alive", &["qwen-2b"]).await;
    let cfg = server(free_port(), "")
        + "\n[[alias]]\nname = \"qwen-2b-any\"\nmodels = [\"phone-2b\", \"qwen-2b\"]\n"
        + &device("d1", "")
        + &device("d2", "")
        + &backend("dead", "d1", dead.port, "chat", "")
        + &backend("alive", "d2", alive.port, "chat", "");
    let h = start_pool(&cfg).await;
    dead.stop();
    h.wait_backend_state("dead", "down", 4).await;
    // Asking for the plain id falls back to the declared alias sibling.
    let r = h.chat("phone-2b").await;
    assert_eq!(r.status(), 200);
    assert_eq!(hdr(&r, "x-vram-pool-model"), "qwen-2b");
    assert_eq!(
        alive.last_body().unwrap()["model"],
        "qwen-2b",
        "the backend must see its own model id"
    );
    // Asking for the alias name resolves too.
    let r = h.chat("qwen-2b-any").await;
    assert_eq!(hdr(&r, "x-vram-pool-backend"), "alive");
}

#[tokio::test(flavor = "multi_thread")]
async fn embeddings_route_like_chat() {
    let (_a, _b, h) = two_chat(&["emb"], &["x"]).await;
    let r = h
        .client
        .post(format!("{}/v1/embeddings", h.base))
        .json(&json!({"model": "emb", "input": "hi"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["from"], "alpha");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_model_is_404_not_503() {
    let (_a, _b, h) = two_chat(&["m"], &["n"]).await;
    let r = h.chat("nope").await;
    assert_eq!(r.status(), 404);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "model_not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn down_devices_report_reason_and_fleet_explanation() {
    let alive = Fake::start("alive", &["m"]).await;
    let dead_port = free_port();
    let fleet = FakeFleet::start(free_port(), &[]).await;
    *fleet.cluster_error.lock().unwrap() =
        Some("cluster engine unreachable: timed out after 4s".into());
    let cfg = server(free_port(), "")
        + &format!("\n[fleet_studio]\nurl = \"{}\"\n", fleet.url())
        + &device("up", "")
        + &device("laptop", "memory = { source = \"none\" }")
        + &device(
            "cluster",
            "memory = { source = \"static\", total_mib = 249090 }",
        )
        + &backend("alive", "up", alive.port, "chat", "")
        + &backend(
            "laptop-qwen",
            "laptop",
            dead_port,
            "chat",
            "models = [\"qwen3.5-2b\"]",
        )
        + &backend(
            "cluster",
            "cluster",
            free_port(),
            "chat",
            "explain_via_fleet = \"cluster\"",
        );
    let h = start_pool(&cfg).await;
    let st = h.get_json("/pool/status").await;
    let laptop = find_device(&st, "laptop").unwrap();
    assert_eq!(laptop["state"], "down");
    assert!(
        laptop["reason"]
            .as_str()
            .unwrap()
            .contains("connection refused"),
        "{laptop}"
    );
    assert_eq!(laptop["memory"]["source"], "none");
    assert_eq!(laptop["memory"]["ok"], false);
    let cl = find_device(&st, "cluster").unwrap();
    assert_eq!(cl["state"], "down");
    let reason = cl["reason"].as_str().unwrap();
    assert!(
        reason.contains("fleet studio")
            && reason.contains("Node B")
            && reason.contains("1/2 nodes up"),
        "{reason}"
    );
    assert_eq!(
        cl["memory"]["total_mib"], 249090,
        "static size stays visible while down"
    );
    // Pooled totals count only devices that are not down; configured_total counts the cluster too.
    assert_eq!(st["pooled"]["devices_total"], 3);
    assert_eq!(st["pooled"]["devices_up"], 1);
    assert_eq!(st["pooled"]["configured_total_mib"], 249090);
    // /v1/models hides down backends by default and shows them flagged with ?all=1.
    let m = h.get_json("/v1/models").await;
    assert!(
        m["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["id"] != "qwen3.5-2b")
    );
    let all = h.get_json("/v1/models?all=1").await;
    let q = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == "qwen3.5-2b")
        .unwrap();
    assert_eq!(q["available"], false);
    assert_eq!(q["residency"], "down");
    assert_eq!(q["device"], "laptop");
    // /health stays 200 (degraded) while one chat backend is up.
    let hr = h
        .client
        .get(format!("{}/health", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(hr.status(), 200);
    assert_eq!(hr.json::<Value>().await.unwrap()["status"], "degraded");
    fleet.stop_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn all_down_makes_health_503() {
    let cfg =
        server(free_port(), "") + &device("d", "") + &backend("a", "d", free_port(), "chat", "");
    let h = start_pool(&cfg).await;
    let r = h
        .client
        .get(format!("{}/health", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_slot_is_idle_not_down() {
    let fleet = FakeFleet::start(free_port(), &[("Tiny-Q4.gguf", 300_000_000)]).await;
    let cfg = server(free_port(), "")
        + &format!("\n[fleet_studio]\nurl = \"{}\"\n", fleet.url())
        + &device("gpu", "")
        + &backend(
            "slot",
            "gpu",
            fleet.slot_port,
            "slot",
            "fleet_host = \"rtx4080\"",
        );
    let h = start_pool(&cfg).await;
    let st = h.get_json("/pool/status").await;
    let s = find_backend(&st, "slot").unwrap();
    assert_eq!(s["state"], "idle");
    assert!(s["reason"].as_str().unwrap().contains("hot-swap"));
    assert_eq!(find_device(&st, "gpu").unwrap()["state"], "idle");
    fleet.stop_all();
}

/// Pool with an always-on chat backend, an empty swap slot, and a fake Fleet Studio.
async fn swap_rig(
    lib: &[(&str, u64)],
    total_mib: u64,
    used_mib: u64,
) -> (Fake, FakeFleet, Harness) {
    let resident = Fake::start("resident", &["resident-model"]).await;
    let fleet = FakeFleet::start(free_port(), lib).await;
    let cfg = server(free_port(), "")
        + &format!("\n[fleet_studio]\nurl = \"{}\"\n", fleet.url())
        + &device(
            "gpu",
            &format!(
                "memory = {{ source = \"static\", total_mib = {total_mib}, used_mib = {used_mib} }}"
            ),
        )
        + &backend("resident", "gpu", resident.port, "chat", "")
        + &backend(
            "slot",
            "gpu",
            fleet.slot_port,
            "slot",
            "fleet_host = \"rtx4080\"",
        );
    let h = start_pool(&cfg).await;
    (resident, fleet, h)
}

#[tokio::test(flavor = "multi_thread")]
async fn hot_swap_is_requested_through_fleet_studio_when_model_not_resident() {
    // 300 MB file needs about 0.33 GiB * 1.15 + 0.6 GiB = ~1 GiB; 8 GiB free.
    let (_resident, fleet, h) = swap_rig(&[("Tiny-Q4.gguf", 300_000_000)], 16_000, 8_000).await;
    let r = h.chat("tiny-q4").await;
    assert_eq!(r.status(), 200, "{:?}", r.text().await);
    assert_eq!(fleet.swap_count(), 1);
    assert_eq!(
        fleet.swaps.lock().unwrap()[0],
        json!({"model": "Tiny-Q4.gguf", "ctx": 8192})
    );
    let r2 = h.chat("slot-4080/Tiny-Q4").await;
    assert_eq!(r2.status(), 200);
    assert_eq!(hdr(&r2, "x-vram-pool-backend"), "slot");
    assert_eq!(
        fleet.swap_count(),
        1,
        "the model is resident now; no second swap"
    );
    let st = h.get_json("/pool/status").await;
    assert_eq!(st["swap"]["succeeded"], 1);
    assert_eq!(st["swap"]["last"]["result"], "ok");
    assert_eq!(find_backend(&st, "slot").unwrap()["state"], "up");
    fleet.stop_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_share_one_swap() {
    let (_resident, fleet, h) = swap_rig(&[("Tiny-Q4.gguf", 300_000_000)], 16_000, 8_000).await;
    fleet.swap_delay_ms.store(500, Ordering::Relaxed);
    let (a, b, c) = tokio::join!(h.chat("tiny-q4"), h.chat("tiny-q4"), h.chat("tiny-q4"));
    for r in [a, b, c] {
        assert_eq!(r.status(), 200);
    }
    assert_eq!(
        fleet.swap_count(),
        1,
        "single flight: three waiters, one swap"
    );
    fleet.stop_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn swap_refused_when_model_does_not_fit() {
    // 20 GB file, 2 GiB free.
    let (_resident, fleet, h) = swap_rig(&[("Huge-Q4.gguf", 20_000_000_000)], 16_000, 14_000).await;
    let r = h.chat("huge-q4").await;
    assert_eq!(
        r.status(),
        503,
        "model is in the library but resident nowhere and does not fit"
    );
    let v: Value = r.json().await.unwrap();
    let msg = v["error"]["message"].as_str().unwrap();
    assert!(msg.contains("needs") && msg.contains("MiB"), "{msg}");
    assert_eq!(
        fleet.swap_count(),
        0,
        "no swap request may reach Fleet Studio when it cannot fit"
    );
    fleet.stop_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_swap_goes_into_cooldown_instead_of_hammering() {
    let (_resident, fleet, h) = swap_rig(&[("Tiny-Q4.gguf", 300_000_000)], 16_000, 8_000).await;
    fleet.swap_status.store(422, Ordering::Relaxed);
    for _ in 0..4 {
        let r = h.chat("tiny-q4").await;
        assert!(r.status() == 503 || r.status() == 404, "{}", r.status());
    }
    assert_eq!(
        fleet.swap_count(),
        1,
        "cooldown holds back repeat swaps of the same model"
    );
    let st = h.get_json("/pool/status").await;
    assert_eq!(st["swap"]["failed"], 1);
    assert_eq!(st["swap"]["last"]["result"], "failed");
    fleet.stop_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn swap_keeps_a_safety_reserve_free_for_other_gpu_residents() {
    // Tiny needs ~943 MiB and 2000 MiB is free, so it fits, but the default 1536 MiB reserve would be
    // violated. The pool must refuse without calling Fleet Studio.
    let (_resident, fleet, h) = swap_rig(&[("Tiny-Q4.gguf", 300_000_000)], 16_000, 14_000).await;
    let r = h.chat("tiny-q4").await;
    assert_eq!(r.status(), 503, "known library file, declined for room");
    let v: Value = r.json().await.unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("safety reserve"),
        "{v}"
    );
    assert_eq!(fleet.swap_count(), 0);
    fleet.stop_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn no_swap_for_unknown_model_names() {
    let (_resident, fleet, h) = swap_rig(&[("Tiny-Q4.gguf", 300_000_000)], 16_000, 8_000).await;
    let r = h.chat("totally-unknown").await;
    assert_eq!(r.status(), 404);
    assert_eq!(fleet.swap_count(), 0);
    fleet.stop_all();
}

#[tokio::test(flavor = "multi_thread")]
async fn guards_reject_foreign_host_origin_and_non_json() {
    let (_a, _b, h) = two_chat(&["m"], &["n"]).await;
    // Foreign Host header.
    let r = h
        .client
        .get(format!("{}/health", h.base))
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // Cross-origin POST.
    let r = h
        .client
        .post(format!("{}/v1/chat/completions", h.base))
        .header("origin", "http://evil.example")
        .json(&json!({"model": "m"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // POST that is not JSON.
    let r = h
        .client
        .post(format!("{}/v1/chat/completions", h.base))
        .header("content-type", "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 415);
    // Garbage JSON gets an OpenAI-shaped 400.
    let r = h
        .client
        .post(format!("{}/v1/chat/completions", h.base))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<Value>().await.unwrap()["error"]["code"],
        "invalid_json"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn status_page_is_served_with_locked_down_headers_and_follows_theme() {
    let (_a, _b, h) = two_chat(&["m"], &["n"]).await;
    for path in ["/", "/status"] {
        let r = h
            .client
            .get(format!("{}{path}", h.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{path}");
        assert!(hdr(&r, "content-type").starts_with("text/html"));
        let csp = hdr(&r, "content-security-policy");
        assert!(
            csp.contains("default-src 'none'") && csp.contains("script-src 'self'"),
            "{csp}"
        );
        let body = r.text().await.unwrap();
        assert!(
            body.contains("VRAM Pool")
                && body.contains("/status.js")
                && body.contains("/theme.css")
        );
        assert!(
            !body.contains("<script>"),
            "no inline script is allowed by the CSP"
        );
    }
    let js = h
        .client
        .get(format!("{}/status.js", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(js.status(), 200);
    let js = js.text().await.unwrap();
    assert!(
        js.contains("/pool/status") && !js.contains("innerHTML"),
        "untrusted text must go through textContent"
    );
    let css = h
        .client
        .get(format!("{}/theme.css", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(css.contains("--bg:") && css.contains("--accent:") && css.contains("color-scheme:"));
    // The page is behind the same Host guard as the API.
    let r = h
        .client
        .get(format!("{}/status", h.base))
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_and_status_shapes() {
    let (_a, _b, h) = two_chat(&["m"], &["n"]).await;
    let _ = h.chat("m").await;
    let text = h
        .client
        .get(format!("{}/metrics", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        text.contains("vram_pool_backend_up{backend=\"a\",device=\"dev-a\"} 1"),
        "{text}"
    );
    assert!(
        text.contains("vram_pool_backend_requests_total{backend=\"a\",status=\"200\"} 1"),
        "{text}"
    );
    let st = h.get_json("/pool/status").await;
    for k in [
        "service",
        "version",
        "generated_at",
        "uptime_s",
        "pooled",
        "devices",
        "swap",
    ] {
        assert!(st.get(k).is_some(), "missing {k}");
    }
    let d = find_device(&st, "dev-a").unwrap();
    for k in [
        "id",
        "label",
        "kind",
        "state",
        "reason",
        "role",
        "memory",
        "models_resident",
        "requests_per_s",
        "last_tok_s",
        "backends",
    ] {
        assert!(d.get(k).is_some(), "device missing {k}");
    }
    assert!(st["pooled"].get("total_mib").is_some() && st["pooled"].get("free_mib").is_some());
}

#[tokio::test]
async fn fleet_refuses_swap_requests_for_non_slot_hosts_without_network_io() {
    let fleet = Fleet::new("http://127.0.0.1:1", reqwest::Client::new());

    let err = fleet
        .swap("other-host", "model.gguf", 4096, Duration::from_secs(1))
        .await
        .unwrap_err();

    assert_eq!(err.status, None);
    assert!(err.message.contains(SWAPPABLE_HOST));
}
