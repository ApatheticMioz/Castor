# vLLM Serving Session Telemetry & Benchmark Traces
**Session Date**: 2026-08-24
**Session Inception**: 2026-08-24 00:48:15 (21+ hours continuous runtime)
**Timestamp Captured**: 2026-08-24 22:15:00

---

## 1. Engine & Model Configuration
- **Model**: `Qwen3.8-27B` (W4A16 AutoRound, int8 quantized `lm_head` & `embed_tokens`)
- **Serving Architecture**: vLLM 0.27.1 + KVarN dense backend + DFlash2 block drafter
- **Context Config**: `CTX=huge` (245,760 max-model-len, 268,169 KV pool capacity)
- **KV Cache Format**: KVarN `k4v2_g128` (4-bit Keys / 2-bit Values per 128-token tile)
- **Recurrent State**: `float16` for Gated-DeltaNet SSM layers
- **Speculative Decoding**: DFlash2 (`SPEC=dflash2`, 7 draft tokens/step block drafter)
- **Prefix Caching**: Enabled (`PREFIX_CACHE=1`, unit 128, hash sha256)
- **Max Concurrency**: `MAX_SEQS=8` (CUDA Graph capture sizes: 64)
- **Default Reasoning Effort**: `medium` (server-side chat template default)

---

## 2. Session Aggregate Telemetry (Overnight & Day Run)

| Metric | Measured Value | Description |
| :--- | :--- | :--- |
| **Total API Invocations** | **3,758 requests** | POST `/v1/chat/completions` successful (2xx) |
| **Total Prompt / KV Computed Tokens** | **8,805,026 tokens** (~8.81M) | Total tokens prefilled & computed across session |
| **Active Decode Time** | **51,581.7 seconds** (~14.33 hrs) | Pure token generation execution time |
| **Active Prefill Time** | **18,021.9 seconds** (~5.01 hrs) | Prompt ingestion & KV computation time |
| **Speculative Draft Passes** | **797,725 passes** | DFlash2 block drafting passes |
| **Speculative Tokens Accepted** | **2,255,669 tokens** (~2.26M) | Accepted speculative tokens |
| **Speculative Acceptance Multiplier**| **~2.83 tokens / draft step** | High effective generation acceleration |
| **Engine Process Resident RAM** | **1.75 GiB** | Host RAM allocated to python process |
| **GPU VRAM Utilization** | **24,256 MiB / 24,576 MiB** | 0.93 pinned allocation on RTX 3090 |

---

## 3. Associated Artifacts in this Directory
* [`vllm_metrics.prom`](vllm_metrics.prom) — Complete Prometheus metric series dump.
* [`vllm_metrics_parsed.json`](vllm_metrics_parsed.json) — 520 parsed metric series (counters, gauges, histogram sums/counts).
* [`vllm_models.json`](vllm_models.json) — Authenticated `/v1/models` endpoint output.
* [`server_config.json`](server_config.json) — Process cmdline arguments, flags, and environment variables.
* [`gpu_snapshot.txt`](gpu_snapshot.txt) — GPU power, memory, and temperature profile.
