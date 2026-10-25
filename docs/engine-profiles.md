# Serving Engine Profiles & Capability Model

Castor is an **engine-agnostic autonomous microkernel**. The underlying model (Bonsai, Qwen, DeepSeek, or any open-weights / proprietary model) and the serving engine (`llama.cpp`, `vLLM`, `SGLang`, `Ollama`, `LM Studio`, or a generic OpenAI-compatible API) are **pluggable profiles**, not system identity.

The microkernel's core invariants—5-layer zero-trust sandbox, structural AST surgery, task registry, telemetry ledger, and evolutionary optimizer—remain invariant regardless of the serving engine. Only the **Serving Engine Profile** adapts to the target engine's wire protocol and hardware topology.

---

## 1. Architecture Overview

```
Lead Architect (Cloud: Claude / Gemini / Antigravity)
        │
        │  MCP over stdio (castor_coworker, castor_task, castor_server)
        ▼
Castor Microkernel (Rust)
        ├── 5-Layer Security Sandbox & AST Surgery
        ├── File-Lease Cross-Process Semaphore (:18021 status server)
        ├── Zero-Turn Stream Proxy (:18022 SSE reassembly)
        └── Pluggable Engine Driver (`src/engine/backend.rs`)
                 │
                 │ Speaks OpenAI-Compatible Chat Completions
                 ▼
Serving Engine Profile (Pluggable Endpoint: port 18020)
        ├── llama.cpp (PRISM CUDA / Native Windows or Linux)
        ├── vLLM (WSL2 / Linux / Docker)
        ├── SGLang / Aphrodite / TGI / Ollama / LM Studio
        └── Remote / Custom OpenAI Endpoints
```

---

## 2. The Engine Capability Model

Rather than maintaining ad-hoc if/else branches across the codebase, Castor models serving engines through a set of orthogonal capability dimensions (`src/engine/backend.rs`):

| Capability Dimension | Meaning | Supported Wire Formats |
|---|---|---|
| `reasoning_wire_format` | How thinking/reasoning tokens are streamed | `DeepSeekField` (`delta.reasoning_content`), `ThinkingTags` (`<think>...</think>`), `None` |
| `effort_strategy` | How reasoning effort is requested from the template | `TopLevel` (`body["reasoning_effort"] = "medium"`), `ChatTemplateKwargs` (`chat_template_kwargs.reasoning_effort`), `None` |
| `stream_sentinel` | Whether stream completion requires an explicit sentinel | `RequiresDone` (`data: [DONE]`), `StandardEof` (HTTP chunked transfer close) |
| `readiness_probe` | How server readiness is verified at boot | `LlamaHealth` (`/health` status + `/models`), `ModelsList` (`/v1/models` check), `TcpConnect` |
| `probe_shape` | Format used for Decomposition Granularity Index (DGI) | `JsonSchemaChat` (structured schema output), `GuidedChoiceCompletion` |
| `is_managed` | Whether Castor manages process boot/canary/shutdown | `true` (managed via `launch_command`), `false` (external/user-managed daemon) |

---

## 3. Configuring an Engine Profile

An engine profile is declared in `~/.castor/config.json` (or via environment variables).

```json
{
  "engine_type": "llama.cpp",
  "base_url": "http://127.0.0.1:18020/v1",
  "model": "Ternary-Bonsai-2-27B-PQ2_0",
  "max_context": 524288,
  "max_concurrent_tasks": 4,
  "launch_command": "cmd /c scripts\\bonsai\\start_bonsai_24gb.bat",
  "stop_command": "taskkill /F /IM llama-server.exe",
  "boot_timeout_secs": 180
}
```

### Key Configuration Keys

- `engine_type`: Selects or aliases the capability profile (`llama.cpp`, `vllm`, `generic`, `openai`, `sglang`, `ollama`, `lmstudio`).
- `base_url`: HTTP(S) endpoint URL of the serving engine.
- `model`: Target model identifier reported by `/v1/models`.
- `max_context`: Context window size in tokens.
- `max_concurrent_tasks`: Concurrency slots allocated for simultaneous worker execution (file-backed lease semaphore).
- `launch_command`: Optional managed startup script.
- `stop_command`: Optional process teardown command.

---

## 4. Shipped Example Profiles

Castor ships with tuned profiles as reference implementations. All profiles are equal citizens in the microkernel harness:

### 4.1 `bonsai-max` (Native Windows llama.cpp)
- **Engine**: PRISM CUDA `llama-server.exe`
- **Model**: `Ternary-Bonsai-2-27B-PQ2_0.gguf` (1.58-bit ternary, 6.70 GiB)
- **Drafter**: `Bonsai-2-27B-DFlash2-Q8_0.gguf` (speculative lookup draft)
- **Concurrency**: 4 dynamic slots (`-np 4`), 524,288 token unified KV pool (`-kvu`)
- **Reasoning**: DeepSeek format via `delta.reasoning_content`

### 4.2 `hyperqwen-huge` (WSL2 / Linux vLLM)
- **Engine**: vLLM OpenAI-compatible server
- **Model**: `qwen3.8-27b`
- **Concurrency**: 1 slot (default), external daemon management
- **Reasoning**: `chat_template_kwargs: {"reasoning_effort": effort}`

### 4.3 `generic` (Ollama / LM Studio / SGLang / Remote API)
- **Engine**: Any OpenAI-compatible chat completion provider
- **Reasoning**: Standard top-level parameters with fallback to content parsing
- **Concurrency**: Configurable cross-process slots via `CASTOR_MAX_CONCURRENT_TASKS`

### 4.4 `bonsai-12gb` (Native Windows llama.cpp, 12GB VRAM)
- **Engine**: PRISM CUDA `llama-server.exe` (native Windows)
- **Model**: `Ternary-Bonsai-2-27B-PQ2_0` (DFlash2 draft) — 1.58-bit ternary base
- **Drafter**: `Bonsai-2-27B-DFlash2-Q8_0` (fine-tuned; falls back to `Qwen3.8-27B-DFlash2-Q4_K_M`)
- **Concurrency**: 1 dedicated slot (`-np 1`)
- **Context**: 65,536 tokens (`-c 65536`), quantized KV `-ctk q4_0 -ctv q4_0`
- **Reasoning**: DeepSeek format via `delta.reasoning_content`
- **Launcher**: `scripts/bonsai/start_bonsai_12gb.bat`

---

## 4a. Hardware / VRAM Matrix — `bonsai-12gb` 12GB Budget

`bonsai-12gb` is the optimized serving profile for **12GB GPUs** (RTX 3060 12GB, RTX 4070 12GB, RTX 3080 12GB), delivering a full 27B ternary model with DFlash2 speculative decoding at **64K native context** (65,536 tokens) 100% in VRAM. Empirically measured total VRAM footprint is **~11.7 GB** (tested live via `nvidia-smi`), fitting inside the 12,288 MiB boundary of 12GB cards.

| Component | Allocation | Notes |
|---|---|---|
| Main weights (`Ternary-Bonsai-2-27B-PQ2_0`, 1.58-bit ternary) | ~6.70 GB | ~100% offload (`-ngl 99`) |
| DFlash2 drafter (`Bonsai-2-27B-DFlash2-Q8_0`, Q8_0) | ~0.50 GB | Fully offloaded (`-ngld 99`), `-sps 0.05` |
| KV cache (`-c 65536`, `-ctk/-ctv q4_0`) | ~4.00 GB | 4-bit K/V at 64K context (64 KB/tok, `-np 1`) |
| CUDA context + activation / scratch buffers | ~0.50 GB | FlashAttention (`-fa`, `-b 2048`, `-ub 512`), dynamic `-lcd` lookup cache |
| **Total Measured VRAM** | **~11.70 GB** | **Fits 100% inside 12GB (12,288 MiB) VRAM** |

**Empirical Ceiling vs 128K:**
- **128K Context (`-c 131072`)**: Measured at **13.23 GB** VRAM (`13,231 MiB`), which spills over 12GB cards by ~940 MiB into system RAM.
- **64K Context (`-c 65536`)**: Measured at **11.72 GB** VRAM net (`12,074 MiB` total with OS overhead), running at **~38.6 tok/s** decode with DFlash2 active.

**Trade-offs vs. the heavier Bonsai profiles:**
- **Single slot only (`-np 1`)**: Avoids the multi-slot memory bandwidth wall and prevents KV cache fragmentation.
- **64K context envelope**: 2x larger than conservative 32K estimates; allows substantial repo code context in VRAM.
- **Keeps the speculative decode win**: Fine-tuned DFlash2 + lookup-augmented speculation (`--spec-type draft-dflash,ngram-mod`, `-lcd`) is retained.
- **Pageable host RAM** (Windows first): inactive pages may be trimmed to the NVMe pagefile
  under host memory pressure, so the profile degrades gracefully rather than hard-OOMing the
  host (see `scripts/bonsai/README.md` §3).

---

## 5. Adding a New Serving Engine

To integrate a new serving backend:

1. **Map the Identifier**: Add any alias to `Backend::from_config` in `src/engine/backend.rs`.
2. **Declare Capabilities**: Define its `EngineCapabilities` (wire format, effort format, readiness probe, sentinel).
3. **Verify Wire Compatibility**: Ensure `/chat/completions` streams clean SSE frames with tool calls.
4. **Add Fixture Test**: Add a test in `tests/llama_cpp.rs` or a dedicated suite validating chat, reasoning stream, and error handling against mock endpoints.
