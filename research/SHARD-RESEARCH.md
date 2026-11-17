# Shard research: running one model across two machines

Date: 2026-10-21. Question: for a model bigger than any single device, which way of splitting it across the
RTX 4080 workstation and the Mac is worth using, and how fast is it? The pool (`vram-pool`) does not shard.
It places whole models. This page records what sharding costs so you can decide when to use it.

**Result in one line: llama.cpp RPC works across the two machines and is 3.8 times slower at decoding than the
Mac alone on a 9B model. The 4080 could not take part in the timed test because it had only 0.6 to 1.5 GiB free.**

## Candidates, with evidence

| Candidate | Pinned | Verdict | Why (file:line in the pinned clone) |
|---|---|---|---|
| **llama.cpp RPC** (`ggml-rpc-server` + `--rpc`) | b11081 `161755f29e41` | **Picked** | Same build already runs on the Mac. Uses our existing GGUF files. CUDA and Metal both supported by one protocol. Default bind loopback, cache off (`tools/rpc/rpc-server.cpp:175-176`). No outbound network code (`ggml/src/ggml-rpc/`). |
| exo | `bc412e2230f3` | Rejected: unsafe and wrong format | API binds `0.0.0.0` (`src/exo/api/listening_socket.py:4`); `trust_remote_code` defaults to true (`src/exo/shared/models/model_cards.py:179`, `src/exo/worker/engines/mlx/constants.py:17`); runs MLX models, not GGUF (README 28, 238-239). |
| distributed-llama | `4074c0f3fc02` | Rejected: no GPU pooling | Powers of two nodes only, `q40` only, own `.m` format (README 48-51). CPU-first, so the 4080 adds only threads. Binds `0.0.0.0` by default (`src/app.cpp:38`). |
| prima.cpp | `288df6a2cc21` | Rejected: cannot load our models | `src/llama.cpp:17003` asserts only `LLM_ARCH_LLAMA` or `LLM_ARCH_QWEN2`; Qwen3 and Qwen3.5 are absent. Falls back to plain llama.cpp for small models (README 67-69). Repo moved from `Lizonghang/` to `OpenCPIL/` (unverified provenance). |
| vLLM + Ray | not cloned | Not applicable to Mac + 4080 | Needs a GPU of the same vendor on every node. Already used for the two-node cluster (TP2), which the pool treats as one device. |

No native-code analyzer was available, so the C++ of the pick was not analyzed. Only its pinned source was read.

## The test

- Model: `Qwen3.5-9B-Q4_K_M.gguf`, 5.28 GiB, 8.95 B parameters (the file already on the Mac).
- Mac M4 Pro, 24 GiB unified, `ggml-rpc-server` b11081 bound to `127.0.0.1:50052`, reached from the workstation through
  `ssh -L 127.0.0.1:50052:127.0.0.1:50052`. No port was opened on the LAN or VPN. Started for the test, stopped after
  (verified: no listener on 50052 on either machine, the Mac's resident `llama-server` pid 1039 untouched).
- Workstation client: `llama-bench` from the b11081 CUDA 13.4 release, `-p 128 -n 64`.
- Mac load average when the baseline and the RPC run started: 2.5 and 2.0 (rule: start only below 24).
- Baseline measured with the same tool on the Mac itself: `llama-bench -ngl 99`.

### Numbers (tokens per second, mean ± stddev, 2 to 3 repeats)

| Setup | Prompt (pp128) | Decode (tg64) | Notes |
|---|---:|---:|---|
| Mac alone, native Metal | 321.98 ± 5.55 | **36.36 ± 0.69** | baseline, 3 repeats |
| Mac alone, through the pool endpoint (llama-server) | n/a | 36.7 | pool adds no measurable overhead |
| All layers on the Mac via RPC from the workstation (`-ngl 99`) | 253.28 ± 19.56 | **9.66 ± 1.23** | 3.8 times slower decode, 1.3 times slower prompt |
| `-ngl 20`: 20 layers on the Mac via RPC, the rest on the workstation CPU | 14.76 ± 1.79 | 1.70 ± 0.37 | CPU half starved, see caveats |
| Workstation CPU only (`-ngl 0`), 32 tokens | 9.22 ± 0.18 | 0.95 ± 0.10 | for scale |
| RTX 4080 alone, or 4080 + Mac split | not measured | not measured | no free VRAM, see below |

### Why the 4080 did not take part

Free VRAM on the 4080 stayed between 584 and 1656 MiB for the whole 10-minute sample window (60 samples, 10 s apart,
`nvidia-smi`). It holds a small chat model (1.8 GiB), a speech-to-text service
(5.1 GiB), a classifier (2.3 GiB), ComfyUI, a screen recorder and the desktop. The 9B file is 5.3 GiB, so even a thin slice
plus a CUDA context and compute buffers (about 1 GiB) would have used all of the headroom. Earlier the same GPU ran
out of memory under agent load and the machine hard-reset, so the test did not take the risk. This is the same fact the
pool's hot-swap guard encodes: a swap needs the model size plus a 1536 MiB reserve free.

### What the numbers say

1. **RPC over an ssh tunnel costs about 75 ms per generated token** (103.5 ms per token against 27.5 ms native). That is
   consistent with the RPC backend making many small round trips per token; at a few milliseconds of network latency each,
   they dominate. A split between two machines pays off only when the model cannot fit on either one alone.
2. Prompt processing suffers far less (1.3 times) because it moves large batches.
3. A 9B model fits on the Mac alone, so sharding it is the wrong tool. The pool places it on the Mac instead and gets
   36 tokens per second.
4. Never split a model that fits. Use RPC only for something like the 27B IQ4_XS in the 4080 library
   (`Qwen3.8-27B-Uncensored-noMTP-IQ4_XS`) once the 4080 is free, or for a model larger than the Mac's 24 GiB.

### Caveats (read before quoting any number)

- The 4080 is untested. Nothing here predicts 4080 + Mac speed except the per-token round-trip cost, which should be the
  same order because it comes from the transport, not the device.
- Network: the VPN reported 3 ms to the Mac at the start and 49 ms on a later ping, via the LAN path in both cases.
  The tunnel adds ssh encryption and one more hop. A direct wired link would be faster. Not measured, because that would
  mean exposing an unauthenticated RPC port beyond loopback.
- Workstation CPU numbers are low because the shell was limited to 3 CPUs (`nproc` printed 3) and the machine load average
  was 11 to 36 from other jobs. They show the shape, not the capability of the CPU.
- Two to three repeats per row, no warm-up control beyond llama-bench's own. The 20-layer split was run once.
- RPC has no authentication or encryption; upstream labels it proof of concept. The loopback bind plus ssh tunnel is
  the only configuration used.

## Recommendation

- Keep the pool as a placement layer. Do not shard models that fit on one device.
- For a model bigger than one device, try llama.cpp RPC again when (a) the 4080 has at least 8 GiB free, and (b) the two
  machines share a low-latency wired link. Thunderbolt 5 RDMA between Apple Silicon Macs is documented upstream
  (`tools/rpc/README.md`, "RDMA transport") and is the route to watch; it does not apply to the 4080.
- If one 70B-class model is wanted, the two-node cluster already has the memory (2 x 124 GiB) and is the better home.
