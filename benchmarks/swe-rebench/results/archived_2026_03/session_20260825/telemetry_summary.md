# vLLM Serving Session Telemetry & Benchmark Traces (Session 2)
**Session Date**: 2026-08-25
**Session Inception**: 2026-08-25 00:08:08 (Over 15 hours continuous runtime)
**Timestamp Captured**: 2026-08-25T15:29:15.918199

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

## 2. Session Aggregate Telemetry (Batch 2: Tasks 43–50 + Overnight Serving)

| Metric | Measured Value | Description |
| :--- | :--- | :--- |
| **Total API Invocations** | **545 requests** | POST `/v1/chat/completions` successful (2xx) |
| **Total Prompt / KV Computed Tokens** | **1,217,977 tokens** | Total tokens prefilled & computed across session |
| **Active Decode Time** | **12,746.3 seconds** (~3.54 hrs) | Pure token generation execution time |
| **Active Prefill Time** | **4,061.6 seconds** (~1.13 hrs) | Prompt ingestion & KV computation time |
| **Speculative Draft Passes** | **137,430 passes** | DFlash2 block drafting passes |
| **Speculative Tokens Accepted** | **400,233 tokens** | Accepted speculative tokens |
| **Speculative Acceptance Multiplier**| **~2.91 tokens / draft step** | Acceleration over autoregressive baseline |
| **Engine Process Resident RAM** | **1.58 GiB** | Host RAM allocated to python process |
| **GPU VRAM Allocation** | **24.2 GB (0.93 util)** | Pinned allocation (268,169 KV pool capacity) |

---

## 3. Associated Artifacts in this Directory
* [`vllm_metrics.prom`](vllm_metrics.prom) — Complete Prometheus metric series dump.
* [`vllm_metrics_parsed.json`](vllm_metrics_parsed.json) — Parsed metric series.
* [`vllm_models.json`](vllm_models.json) — Authenticated `/v1/models` endpoint output.
* [`server_config.json`](server_config.json) — Process cmdline arguments, flags, and environment variables.
* [`gpu_snapshot.txt`](gpu_snapshot.txt) — GPU power, memory, and temperature profile.
