//! Health, model-list and memory probes, and the loops that run them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::join_all;
use serde_json::Value;

use crate::config::{MemorySource, ModelsSource, Role};
use crate::state::{Backend, Device, GpuApp, HealthState, MemState, ModelEntry, Pool, write};
use crate::util::{self, describe_http_error, is_refused, now_iso, one_line};

const PROBE_HARD_CAP: Duration = Duration::from_secs(20);

/// Spawns the health and memory loops. They run until the process exits. The first tick comes one
/// period from now: the caller already ran `probe_all_once`.
pub fn spawn_loops(pool: &Arc<Pool>) {
    let p = pool.clone();
    tokio::spawn(async move {
        let period = Duration::from_secs(p.cfg.server.health_interval_s);
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            probe_all_backends(&p).await;
        }
    });
    let p = pool.clone();
    tokio::spawn(async move {
        let period = Duration::from_secs(p.cfg.server.memory_interval_s);
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            probe_all_memory(&p).await;
        }
    });
}

/// One full round of both probes. Used at startup so the first request sees real state.
pub async fn probe_all_once(pool: &Arc<Pool>) {
    tokio::join!(probe_all_backends(pool), probe_all_memory(pool));
}

pub async fn probe_all_backends(pool: &Arc<Pool>) {
    let futs = pool.backends.iter().map(|b| {
        let pool = pool.clone();
        let b = b.clone();
        async move {
            if tokio::time::timeout(PROBE_HARD_CAP, probe_backend(&pool, &b))
                .await
                .is_err()
            {
                apply(
                    &pool,
                    &b,
                    HealthState::Down,
                    Some("probe hung past 20s".to_owned()),
                    None,
                )
                .await;
            }
        }
    });
    join_all(futs).await;
}

pub async fn probe_all_memory(pool: &Arc<Pool>) {
    let futs = pool.devices.iter().map(probe_memory);
    join_all(futs).await;
}

/// Probes one backend now. Also called right after a hot-swap.
pub async fn probe_backend(pool: &Arc<Pool>, b: &Arc<Backend>) {
    let started = Instant::now();
    let key = b.key.get().await;
    let timeout = Duration::from_secs(pool.cfg.server.probe_timeout_s);
    let url = format!("{}{}", b.cfg.url, b.cfg.health_path);
    let mut req = pool.probe_http.get(&url);
    if let Some(k) = &key {
        req = req.bearer_auth(k);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let latency = Some(started.elapsed().as_millis() as u64);
            if b.role() == Role::Slot && is_refused(&e) {
                write(&b.models).clear();
                apply(
                    pool,
                    b,
                    HealthState::Idle,
                    Some("slot empty: nothing loaded, hot-swap can fill it".to_owned()),
                    latency,
                )
                .await;
            } else {
                apply(
                    pool,
                    b,
                    HealthState::Down,
                    Some(describe_http_error(&e, timeout)),
                    latency,
                )
                .await;
            }
            return;
        }
    };
    let latency = Some(started.elapsed().as_millis() as u64);
    let status = resp.status();
    if !status.is_success() {
        if status.as_u16() == 401 || status.as_u16() == 403 {
            b.key.invalidate();
        }
        let body = resp.text().await.unwrap_or_default();
        let detail = if body.is_empty() {
            String::new()
        } else {
            format!(": {}", one_line(&body, 100))
        };
        apply(
            pool,
            b,
            HealthState::Down,
            Some(format!("HTTP {}{detail}", status.as_u16())),
            latency,
        )
        .await;
        return;
    }
    let health_body = resp.text().await.unwrap_or_default();

    let models = match b.cfg.models_source {
        ModelsSource::HealthModel => health_model(&health_body, b),
        ModelsSource::V1Models => list_models(pool, b, key.as_deref(), timeout).await,
    };
    match models {
        Ok(list) if list.is_empty() => {
            apply(
                pool,
                b,
                HealthState::Down,
                Some("serves no models".to_owned()),
                latency,
            )
            .await;
        }
        Ok(list) => {
            *write(&b.models) = list;
            apply(pool, b, HealthState::Up, None, latency).await;
        }
        Err(why) => {
            apply(
                pool,
                b,
                HealthState::Down,
                Some(format!("model list: {why}")),
                latency,
            )
            .await;
        }
    }
}

fn health_model(body: &str, b: &Backend) -> Result<Vec<ModelEntry>, String> {
    let v: Value =
        serde_json::from_str(body).map_err(|_| "health response is not JSON".to_owned())?;
    let id = v
        .get("model")
        .and_then(Value::as_str)
        .ok_or("health JSON has no model field")?;
    Ok(vec![ModelEntry {
        id: id.to_owned(),
        ctx: b.cfg.ctx,
        size_bytes: None,
    }])
}

async fn list_models(
    pool: &Arc<Pool>,
    b: &Arc<Backend>,
    key: Option<&str>,
    timeout: Duration,
) -> Result<Vec<ModelEntry>, String> {
    let url = format!("{}/v1/models", b.cfg.url);
    let mut req = pool.probe_http.get(&url);
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| describe_http_error(&e, timeout))?;
    if !resp.status().is_success() {
        if matches!(resp.status().as_u16(), 401 | 403) {
            b.key.invalidate();
        }
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let v: Value = resp.json().await.map_err(|_| "not JSON".to_owned())?;
    let mut out = parse_models(&v, b.cfg.ctx);
    if out.iter().any(|m| m.ctx.is_none()) {
        let ctx = props_ctx(pool, b, key).await;
        for m in &mut out {
            if m.ctx.is_none() {
                m.ctx = ctx;
            }
        }
    }
    Ok(out)
}

/// OpenAI-style `data[]`; llama.cpp adds `meta.n_ctx` and `meta.size`.
pub fn parse_models(v: &Value, default_ctx: Option<u64>) -> Vec<ModelEntry> {
    v.get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?.to_owned();
                    let meta = m.get("meta");
                    let ctx = meta
                        .and_then(|x| x.get("n_ctx"))
                        .and_then(Value::as_u64)
                        .or_else(|| m.get("max_model_len").and_then(Value::as_u64))
                        .or(default_ctx);
                    let size_bytes = meta.and_then(|x| x.get("size")).and_then(Value::as_u64);
                    Some(ModelEntry {
                        id,
                        ctx,
                        size_bytes,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn props_ctx(pool: &Arc<Pool>, b: &Arc<Backend>, key: Option<&str>) -> Option<u64> {
    let mut req = pool.probe_http.get(format!("{}/props", b.cfg.url));
    if let Some(k) = key {
        req = req.bearer_auth(k);
    }
    let v: Value = req.send().await.ok()?.json().await.ok()?;
    v.get("default_generation_settings")?.get("n_ctx")?.as_u64()
}

/// Records health, logs transitions, and resets the breaker when a backend comes back.
async fn apply(
    pool: &Arc<Pool>,
    b: &Arc<Backend>,
    state: HealthState,
    mut reason: Option<String>,
    latency_ms: Option<u64>,
) {
    if state == HealthState::Down
        && let (Some(host), Some(fleet)) = (b.cfg.explain_via_fleet.as_deref(), pool.fleet.as_ref())
        && let Some(why) = fleet.explain(host).await
    {
        reason = Some(format!(
            "{} | {why}",
            reason.unwrap_or_else(|| "down".to_owned())
        ));
    }
    if let Some((prev, now)) = b.set_health(state, reason.clone(), latency_ms) {
        tracing::info!(backend = %b.id(), from = prev.as_str(), to = now.as_str(), reason = ?reason, "backend state changed");
        if now == HealthState::Up {
            b.breaker.reset();
        }
    }
}

// ---- memory ----

pub async fn probe_memory(d: &Arc<Device>) {
    let cfg = &d.cfg.memory;
    let mut state = match cfg.source {
        MemorySource::NvidiaSmi => nvidia(cfg.gpu_index).await,
        MemorySource::SshMacos => mac(cfg.ssh.as_deref().unwrap_or_default()).await,
        MemorySource::Static => static_mem(cfg.total_mib, cfg.used_mib),
        MemorySource::None => MemState {
            ok: false,
            error: Some("device cannot report memory and no size is configured".to_owned()),
            ..MemState::default()
        },
    };
    state.checked_at = Some(now_iso());
    *write(&d.memory) = state;
}

/// Fixed size from config. `used_mib` is optional; without it used and free stay unknown.
pub fn static_mem(total: Option<u64>, used: Option<u64>) -> MemState {
    MemState {
        total_mib: total,
        used_mib: used,
        free_mib: total.zip(used).map(|(t, u)| t.saturating_sub(u)),
        ok: total.is_some(),
        ..MemState::default()
    }
}

async fn nvidia(index: Option<u32>) -> MemState {
    let idx = index.map(|i| i.to_string());
    let mut argv = vec!["nvidia-smi"];
    if let Some(i) = &idx {
        argv.extend(["-i", i.as_str()]);
    }
    argv.extend([
        "--query-gpu=memory.total,memory.used,memory.free",
        "--format=csv,noheader,nounits",
    ]);
    let r = util::run(&argv, Duration::from_secs(8)).await;
    if r.code != 0 {
        return MemState {
            error: Some(one_line(
                &format!(
                    "nvidia-smi failed: {}",
                    if r.stderr.is_empty() {
                        &r.stdout
                    } else {
                        &r.stderr
                    }
                ),
                160,
            )),
            ..MemState::default()
        };
    }
    let Some((total, used, free)) = parse_nvidia_smi(&r.stdout) else {
        return MemState {
            error: Some("nvidia-smi output not understood".to_owned()),
            ..MemState::default()
        };
    };
    let mut app_argv = vec!["nvidia-smi"];
    if let Some(i) = &idx {
        app_argv.extend(["-i", i.as_str()]);
    }
    app_argv.extend([
        "--query-compute-apps=pid,process_name,used_memory",
        "--format=csv,noheader,nounits",
    ]);
    let apps = {
        let a = util::run(&app_argv, Duration::from_secs(8)).await;
        if a.code == 0 {
            parse_compute_apps(&a.stdout)
        } else {
            Vec::new()
        }
    };
    MemState {
        total_mib: Some(total),
        used_mib: Some(used),
        free_mib: Some(free),
        ok: true,
        apps,
        ..MemState::default()
    }
}

/// Sums every GPU line of `memory.total,memory.used,memory.free` in MiB.
pub fn parse_nvidia_smi(out: &str) -> Option<(u64, u64, u64)> {
    let mut sum = (0u64, 0u64, 0u64);
    let mut n = 0;
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let nums: Vec<u64> = line
            .split(',')
            .filter_map(|p| p.trim().parse::<u64>().ok())
            .collect();
        if nums.len() != 3 {
            return None;
        }
        sum.0 += nums[0];
        sum.1 += nums[1];
        sum.2 += nums[2];
        n += 1;
    }
    (n > 0).then_some(sum)
}

pub fn parse_compute_apps(out: &str) -> Vec<GpuApp> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split(',').map(str::trim);
            let pid = it.next()?.parse().ok()?;
            let name = it.next()?;
            let used = it.next()?.parse().ok()?;
            let base = name.rsplit('/').next().unwrap_or(name).to_owned();
            Some(GpuApp {
                pid,
                name: base,
                used_mib: used,
            })
        })
        .collect()
}

async fn mac(host: &str) -> MemState {
    let cmd = "echo __M; sysctl -n hw.memsize; echo __P; memory_pressure | tail -1; echo __L; sysctl -n vm.loadavg; echo __E";
    let r = util::ssh(host, cmd, Duration::from_secs(12)).await;
    if !r.stdout.contains("__E") {
        let why = if r.stderr.trim().is_empty() {
            format!("exit {}", r.code)
        } else {
            r.stderr
        };
        return MemState {
            error: Some(one_line(&format!("ssh {host}: {why}"), 160)),
            ..MemState::default()
        };
    }
    match parse_mac(&r.stdout) {
        Some(m) => m,
        None => MemState {
            error: Some("mac memory output not understood".to_owned()),
            ..MemState::default()
        },
    }
}

/// `memsize` bytes, `memory_pressure` last line ("... free percentage: 84%"), `vm.loadavg` "{ 1.79 1.76 8.86 }".
pub fn parse_mac(out: &str) -> Option<MemState> {
    let mut sec: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut cur = None;
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(name) = line.strip_prefix("__") {
            cur = Some(name);
            sec.entry(name).or_default();
        } else if let Some(c) = cur {
            sec.entry(c).or_default().push(line);
        }
    }
    let bytes: u64 = sec.get("M")?.first()?.parse().ok()?;
    let total_mib = bytes / (1024 * 1024);
    let pct: u64 = sec
        .get("P")?
        .first()?
        .rsplit(|c: char| !c.is_ascii_digit() && c != '%')
        .find(|t| t.ends_with('%'))?
        .trim_end_matches('%')
        .parse()
        .ok()?;
    let free = total_mib * pct.min(100) / 100;
    let load1 = sec.get("L").and_then(|v| v.first()).and_then(|l| {
        l.trim_matches(|c| c == '{' || c == '}' || c == ' ')
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<f64>().ok())
    });
    Some(MemState {
        total_mib: Some(total_mib),
        used_mib: Some(total_mib - free),
        free_mib: Some(free),
        ok: true,
        load1,
        ..MemState::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_parse_sums_gpus() {
        assert_eq!(
            parse_nvidia_smi("16376, 12938, 3056\n"),
            Some((16376, 12938, 3056))
        );
        assert_eq!(
            parse_nvidia_smi("100, 40, 60\n200, 50, 150\n"),
            Some((300, 90, 210))
        );
        assert_eq!(parse_nvidia_smi(""), None);
        assert_eq!(parse_nvidia_smi("garbage"), None);
    }

    #[test]
    fn compute_apps_parse() {
        let apps = parse_compute_apps("1667, /opt/x/llama-server, 1848\n1605, python, 5088\n");
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "llama-server");
        assert_eq!(apps[1].used_mib, 5088);
    }

    #[test]
    fn mac_parse() {
        let out = "__M\n25769803776\n__P\nSystem-wide memory free percentage: 84%\n__L\n{ 1.79 1.76 8.86 }\n__E\n";
        let m = parse_mac(out).expect("parses");
        assert_eq!(m.total_mib, Some(24576));
        assert_eq!(m.free_mib, Some(24576 * 84 / 100));
        assert_eq!(m.used_mib, Some(24576 - 24576 * 84 / 100));
        assert_eq!(m.load1, Some(1.79));
        assert!(parse_mac("__M\nnope\n__E\n").is_none());
    }

    #[test]
    fn models_parse_llamacpp_meta() {
        let v: Value = serde_json::from_str(
            r#"{"data":[{"id":"a","meta":{"n_ctx":16384,"size":1269873920}},{"id":"b"}]}"#,
        )
        .unwrap();
        let m = parse_models(&v, Some(4096));
        assert_eq!(m[0].ctx, Some(16384));
        assert_eq!(m[0].size_bytes, Some(1269873920));
        assert_eq!(m[1].ctx, Some(4096));
    }
}
