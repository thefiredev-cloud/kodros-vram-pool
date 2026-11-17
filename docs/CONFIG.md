# Configuration

Config file: `~/.config/vram-pool/pool.toml`, or the path given with `--config`. The shipped default is `deploy/pool.toml`, commented line by line.

| Setting | Default | Effect |
|---|---|---|
| `HOME` (env) | none | Locates the default config path and the theme file the status page follows. |
| `RUST_LOG` (env) | `vram_pool=info` | Log filter. The service unit sets `vram_pool=info`. |
| `$HOME/.local/state/omarchy/current/theme/colors.toml` | absent | Optional theme file read by the status page. |
| `[server] bind` | `127.0.0.1:8100` | Listen address. Any non-loopback address is refused at start-up. |
| `[server] health_interval_s` | `5` | Seconds between backend health probes. |
| `[server] memory_interval_s` | `10` | Seconds between device memory probes. |
| `[server] probe_timeout_s` | `3` | Timeout of one health or model-list probe. |
| `[server] connect_timeout_s` | `3` | TCP connect timeout for proxied requests. |
| `[server] request_timeout_s` | `120` | Wait for response headers from a backend. |
| `[server] stream_idle_timeout_s` | `60` | A stream silent for this long is cut. |
| `[server] max_attempts` | `3` | Backends tried per request, each at most once. |
| `[server] breaker_failures` | `3` | Consecutive failures that open a circuit. |
| `[server] breaker_open_s` | `20` | First open period. Doubles on a failed half-open trial. |
| `[server] breaker_max_open_s` | `120` | Upper bound of the open period. |
| `[server] metrics` | `true` | Serve `GET /metrics`. |
| `[server] max_body_mib` | `64` | Largest request body. |
| `[fleet_studio] url` | none | Base URL of Fleet Studio. Required for hot-swap. |
| `[swap] enabled` | `true` | Allow the one slot swap. |
| `[swap] ctx` | `8192` | Context size requested for a swapped-in model. |
| `[swap] vram_factor` | `1.15` | Fit check factor, mirrors Fleet Studio's preflight. |
| `[swap] overhead_gib` | `0.6` | Fit check overhead, mirrors Fleet Studio's preflight. |
| `[swap] reserve_mib` | `1536` | Memory that must stay free after a swap. |
| `[swap] timeout_s` | `330` | Wait for Fleet Studio to finish one swap. |
| `[swap] cooldown_s` | `120` | After a failed or refused swap, do not ask again for this long. |

Entries that have no default:

| Entry | Fields |
|---|---|
| `[[alias]]` | `name`, `models` (ordered ids, first resident one wins). |
| `[[device]]` | `id`, `label`, `kind` (`gpu`, `unified`, `cluster`), `memory` with `source` (`nvidia-smi`, `ssh-macos`, `static`, `none`; default `none`) and optional `total_mib`, `used_mib`, `ssh`, `gpu_index`, `note`. |
| `[[backend]]` | `id`, `device`, `url`, `role` (`chat`, `slot`, `decision`, `embedding`), and optional `engine`, `health_path` (default `/health`), `models_source` (`v1-models` default, or `health-model`), `key_file`, `key_ssh` (`host`, `path`), `ctx`, `models`, `fleet_host`, `explain_via_fleet`, `note`. |

Backend keys come from `key_file` or from `ssh HOST cat PATH`. They are held in memory only and never logged or shown by `/pool/status`.
