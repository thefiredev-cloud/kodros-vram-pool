# Build guide: kodros-vram-pool

## You need

- Rust 1.88 or newer (edition 2024), with `cargo`.
- At least one OpenAI-compatible backend to point it at, such as `llama-server` on a GPU machine.
- Linux with systemd if you want the user service. The binary itself builds on any platform Rust supports.

## Build

```sh
git clone https://github.com/thefiredev-cloud/kodros-vram-pool.git
cd kodros-vram-pool
cargo build --release
```

A cold release build took 1 minute 21 seconds on 2026-11-18. The binary is `target/release/vram-pool`.

## Test

```sh
cargo clippy --all-targets -- -D warnings
cargo test
```

On 2026-11-18 clippy was clean and `cargo test` ran 19 unit tests and 26 integration tests, all passing.

## Configure

```sh
./target/release/vram-pool --init     # writes ~/.config/vram-pool/pool.toml
./target/release/vram-pool --check    # validates it and prints the device and backend counts
```

`--init` copies the example from `deploy/pool.toml`. Replace its `[[device]]` and `[[backend]]` entries with your own machines before you rely on it. Every setting is listed in [docs/CONFIG.md](docs/CONFIG.md).

## Run

```sh
./target/release/vram-pool
curl -s http://127.0.0.1:8100/v1/models
curl -s http://127.0.0.1:8100/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"hello"}]}'
curl -s http://127.0.0.1:8100/pool/status
```

`model` can be a model id, an alias from the config, or `auto`, which picks the fastest healthy chat backend. A status page is at `/status` and Prometheus metrics are at `/metrics`.

The server refuses to start on `0.0.0.0` or any non-loopback address.

## Run as a service

The unit runs `~/.local/bin/vram-pool --config ~/.config/vram-pool/pool.toml`, so run `--init` first.

```sh
mkdir -p ~/.local/bin ~/.config/systemd/user
cp target/release/vram-pool ~/.local/bin/
cp deploy/vram-pool.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now vram-pool
```

## Troubleshooting

- `--check` reports an error: the message names the config key. Fix it and run `--check` again.
- A model request fails with no backend: no healthy backend lists that model. Check `/pool/status`.
- The JSON and error contract is in [docs/POOL-API.md](docs/POOL-API.md).
