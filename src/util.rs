//! Small shared helpers: time, key files, bounded subprocesses.

use std::time::Duration;

use tokio::process::Command;

pub const MIB: f64 = 1_048_576.0;

pub fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// Comparable form of a model id: lowercase, any `prefix/` removed, `.gguf` removed. Lets a
/// request for `qwen3-1.7b-q4_k_m` find the slot's `slot-4080/Qwen3-1.7B-Q4_K_M`.
pub fn norm_model(s: &str) -> String {
    let base = s.rsplit('/').next().unwrap_or(s).to_ascii_lowercase();
    base.strip_suffix(".gguf")
        .map(str::to_owned)
        .unwrap_or(base)
}

/// First non-empty, non-comment line of a key file. The value is never logged or serialized.
pub fn read_key_file(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(crate::config::expand(path)).ok()?;
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
}

pub struct RunResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Runs a fixed argv with a timeout. Spawn failure gives code -1 and the error in `stderr`.
/// The child is killed when the timeout fires.
pub async fn run(argv: &[&str], timeout: Duration) -> RunResult {
    let Some((prog, args)) = argv.split_first() else {
        return RunResult {
            code: -1,
            stdout: String::new(),
            stderr: "empty argv".into(),
            timed_out: false,
        };
    };
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RunResult {
                code: -1,
                stdout: String::new(),
                stderr: format!("spawn {prog}: {e}"),
                timed_out: false,
            };
        }
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => RunResult {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            timed_out: false,
        },
        Ok(Err(e)) => RunResult {
            code: -1,
            stdout: String::new(),
            stderr: e.to_string(),
            timed_out: false,
        },
        Err(_) => RunResult {
            code: -1,
            stdout: String::new(),
            stderr: format!("timed out after {}s", timeout.as_secs()),
            timed_out: true,
        },
    }
}

/// ssh with fixed options and a hard timeout. `remote_cmd` may contain only validated config values.
pub async fn ssh(host: &str, remote_cmd: &str, timeout: Duration) -> RunResult {
    run(
        &[
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
            host,
            remote_cmd,
        ],
        timeout,
    )
    .await
}

/// Short, one-line error text for the status JSON.
pub fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Root cause text of a reqwest error: walks the source chain and classifies the usual cases.
pub fn describe_http_error(e: &reqwest::Error, timeout: Duration) -> String {
    if e.is_timeout() {
        return format!("timed out after {}s", timeout.as_secs());
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    let mut last = e.to_string();
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::ConnectionRefused => "connection refused".to_owned(),
                std::io::ErrorKind::TimedOut => format!("timed out after {}s", timeout.as_secs()),
                std::io::ErrorKind::HostUnreachable | std::io::ErrorKind::NetworkUnreachable => {
                    "host unreachable".to_owned()
                }
                _ => one_line(&io.to_string(), 120),
            };
        }
        last = s.to_string();
        source = s.source();
    }
    one_line(&last, 120)
}

/// True when the error means nothing listens on the port (as opposed to a slow or dead host).
pub fn is_refused(e: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            return io.kind() == std::io::ErrorKind::ConnectionRefused;
        }
        source = s.source();
    }
    false
}
