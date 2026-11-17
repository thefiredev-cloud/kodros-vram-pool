#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use vram_pool::config::{Config, DEFAULT_TOML};
use vram_pool::{probe, state::Pool};

const USAGE: &str = "vram-pool [--config PATH] [--check] [--init] [--print-default]\n\
  --config PATH    config file (default ~/.config/vram-pool/pool.toml)\n\
  --check          validate the config and exit\n\
  --init           write the default config if the file does not exist, then exit\n\
  --print-default  print the default config and exit";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vram_pool=info".into()),
        )
        .with_ansi(false)
        .init();

    let mut path = Config::default_path();
    let (mut check, mut init) = (false, false);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => match args.next() {
                Some(p) => path = PathBuf::from(p),
                None => return fail("--config needs a path"),
            },
            "--check" => check = true,
            "--init" => init = true,
            "--print-default" => {
                print!("{DEFAULT_TOML}");
                return ExitCode::SUCCESS;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => return fail(&format!("unknown argument {other:?}\n{USAGE}")),
        }
    }

    if init {
        if path.exists() {
            println!("{} already exists; left alone", path.display());
            return ExitCode::SUCCESS;
        }
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            return fail(&format!("create {}: {e}", dir.display()));
        }
        if let Err(e) = std::fs::write(&path, DEFAULT_TOML) {
            return fail(&format!("write {}: {e}", path.display()));
        }
        println!("wrote {}", path.display());
        return ExitCode::SUCCESS;
    }

    let cfg = match Config::load(&path) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    if check {
        println!(
            "{}: ok ({} devices, {} backends)",
            path.display(),
            cfg.devices.len(),
            cfg.backends.len()
        );
        return ExitCode::SUCCESS;
    }
    let addr = match cfg.bind_addr() {
        Ok(a) => a,
        Err(e) => return fail(&e),
    };
    let pool = match Pool::new(cfg) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };

    // One probe round first (at most 10 s), so the first request sees real health instead of
    // "not probed yet". Slow probes simply finish in the background loops.
    if tokio::time::timeout(
        std::time::Duration::from_secs(10),
        probe::probe_all_once(&pool),
    )
    .await
    .is_err()
    {
        tracing::warn!("first probe round did not finish in 10s; serving anyway");
    }
    probe::spawn_loops(&pool);

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return fail(&format!("bind {addr}: {e}")),
    };
    tracing::info!(%addr, devices = pool.devices.len(), backends = pool.backends.len(), "vram-pool listening");
    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("no SIGTERM handler: {e}");
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
        tracing::info!("shutting down");
    };
    match vram_pool::serve(pool, listener, shutdown).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&format!("server: {e}")),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("vram-pool: {msg}");
    ExitCode::from(2)
}
