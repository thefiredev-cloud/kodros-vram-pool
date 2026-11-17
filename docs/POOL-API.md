# vram-pool API contract

Version 1, matches `vram-pool 0.1.0`. Base URL `http://127.0.0.1:8100`. Loopback only: the service
refuses to start on `0.0.0.0`, `::` or any non-loopback address. Every request needs a loopback `Host`
header (`127.0.0.1:8100`, `localhost:8100`, `[::1]:8100`). `POST` needs `Content-Type: application/json`
and no foreign `Origin`.

Memory numbers are MiB (1 MiB = 1048576 bytes). Timestamps are RFC 3339 UTC with milliseconds.
`null` means "not known", never "zero".

## OpenAI-compatible routes

| Route | Behavior |
|---|---|
| `POST /v1/chat/completions` | Routed by `model`. `stream: true` is passed through byte for byte (SSE). |
| `POST /v1/completions` | Same routing. |
| `POST /v1/embeddings` | Same routing; embedding backends first. |
| `GET /v1/models` | Union across devices, one entry per model id. `?all=1` adds down and library models. |

`model` may be a model id, an alias name from the config, or `auto` (fastest last measured tok/s among
healthy chat backends, then idlest). Decision models (`role = "decision"`) are listed but never get
chat traffic.

Response headers on every routed reply:

| Header | Value |
|---|---|
| `x-vram-pool-device` | device id that served it |
| `x-vram-pool-backend` | backend id |
| `x-vram-pool-model` | the model id the backend actually served (differs from the request for aliases) |
| `x-vram-pool-attempts` | 1 when the first backend answered, 2 or 3 after failover |

Errors use the OpenAI shape `{"error":{"message","type","code"}}`:

| HTTP | `code` | Meaning |
|---|---|---|
| 400 | `invalid_json` | body is not a JSON object |
| 404 | `model_not_found` | no backend (up or down), config hint or Fleet Studio library file knows the model |
| 503 | `no_backend` | the model is known (seen on a backend, listed in the config, or a library file) but nothing healthy serves it; for a library file the message says why the slot declined (no room, cooldown, Fleet Studio refused) and lists why each backend is down |
| 503 | `backends_failed` | every candidate failed before the first byte; the message lists each reason |
| 403 / 415 | `forbidden_host`, `forbidden_origin`, `unsupported_media_type` | guard |

### `GET /v1/models`

```json
{
  "object": "list",
  "data": [
    {
      "id": "mac-qwen3.5-9b",
      "object": "model",
      "created": 1792626000,
      "owned_by": "mac",
      "device": "mac",
      "devices": ["mac"],
      "backend": "mac-resident",
      "residency": "resident",
      "role": "chat",
      "ctx": 65536,
      "available": true,
      "routable": true,
      "placements": [
        { "device": "mac", "backend": "mac-resident", "residency": "resident",
          "role": "chat", "ctx": 65536, "size_bytes": 5680522464, "available": true }
      ]
    }
  ],
  "aliases": [ { "name": "qwen3.5-2b", "models": ["small-chat-qwen3.5-2b", "qwen3.5-2b"] } ]
}
```

`residency`: `resident` (always-on backend), `slot` (hot-swap slot), `library` (file Fleet Studio could swap
into the 4080 slot, only with `?all=1`), `down` (backend down, last known or configured model, only with
`?all=1`). `available` is false for `library` and `down`. `routable` is false for decision models.

## `GET /pool/status`

The one JSON a UI needs. Shape:

```json
{
  "service": "vram-pool",
  "version": "0.1.0",
  "generated_at": "2026-10-21T23:48:38.145Z",
  "uptime_s": 193,
  "pooled": {
    "devices_total": 4,
    "devices_up": 2,
    "total_mib": 40952,
    "used_mib": 26560,
    "free_mib": 14010,
    "configured_total_mib": 290042,
    "unreported_devices": [],
    "note": "..."
  },
  "devices": [ /* Device */ ],
  "swap": {
    "enabled": true,
    "slot_backend": "slot-4080",
    "requested": 0, "succeeded": 0, "failed": 0,
    "last": null
  }
}
```

`pooled.total_mib`, `used_mib`, `free_mib` sum the devices that are not `down` and report all three
numbers. Devices that are up but report less (static size only) are listed in `unreported_devices`.
`configured_total_mib` also counts down devices with a known size, so a UI can show "pool 40 GiB live,
283 GiB configured".

### Device

| Field | Type | Notes |
|---|---|---|
| `id`, `label` | string | `workstation`, `mac`, `laptop`, `cluster` in the example config |
| `kind` | `gpu` \| `unified` \| `cluster` | |
| `state` | `up` \| `idle` \| `down` | `idle`: only an empty swap slot answers |
| `reason` | string \| null | why it is down; joins each backend's reason, plus Fleet Studio's explanation for the cluster |
| `role` | `chat` \| `embedding` \| `decision` | best role among its backends; `roles` lists all |
| `memory` | object | see below |
| `models_resident` | string[] | model ids served by healthy backends, decision models included |
| `inflight` | int | requests being served right now |
| `requests_per_s` | number | over the last 30 s |
| `last_tok_s` | number \| null | most recent measured generation speed on this device |
| `backends` | Backend[] | |

`memory`: `source` (`nvidia-smi` \| `ssh-macos` \| `static` \| `none`), `total_mib`, `used_mib`,
`free_mib` (each nullable), `ok`, `error`, `checked_at`, `load1` (Mac only), `note`, `apps`
(`[{pid, name, used_mib}]`, nvidia-smi only).

### Backend

`id`, `url`, `role` (`chat` \| `slot` \| `decision` \| `embedding`), `engine`, `state`
(`up` \| `idle` \| `down`), `reason`, `latency_ms` (last probe), `checked_at`, `since` (last state
change), `circuit` (`closed` \| `open` \| `half_open`), `circuit_opened_total`, `inflight`,
`requests_per_s`, `last_tok_s`, `avg_response_ms`, `requests_ok`, `requests_failed`,
`responses_by_status` (`{"200": 12}`), `models` (`[{id, ctx, size_bytes, residency}]`), `note`, `routable`.

### Swap block

`last` is `null` or `{ at, model, file, result, detail, ms }` with `result` one of `ok`, `refused` (the
pool declined: no fit, slot busy, unknown file, cooldown), `failed` (Fleet Studio declined or errored).

## `GET /health`

`200` `{"ok":true,"status":"ok"|"degraded", "chat_backends_up", "devices_up", "devices_total"}` while at
least one chat or slot backend is up. `503` with `"status":"down"` when none is.

## `GET /metrics`

Prometheus text (`metrics = false` in the config removes the route): `vram_pool_device_up`,
`vram_pool_device_memory_mib{kind}`, `vram_pool_backend_up`, `vram_pool_backend_circuit_open`,
`vram_pool_backend_inflight`, `vram_pool_backend_requests_total{status}`,
`vram_pool_backend_failures_total`, `vram_pool_backend_last_tokens_per_second`,
`vram_pool_swap_total{result}`, `vram_pool_http_requests_total{path,code}`.
