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

### 4.4 `bonsai-8gb` (Native Windows llama.cpp, 8GB VRAM)
- **Engine**: PRISM CUDA `llama-server.exe` (native Windows)
- **Model**: `Ternary-Bonsai-2-27B-PQ2_0` (1.58-bit ternary base, 6.70 GB weights)
- **Drafter**: None (Zero-VRAM Dynamic N-Gram Lookup via `-lcd`)
- **Concurrency**: 1 dedicated slot (`-np 1`)
- **Context**: 8,192 tokens (`-c 8192`), quantized KV `-ctk q4_0 -ctv q4_0`
- **Reasoning**: DeepSeek format via `delta.reasoning_content`
- **Launcher**: `scripts/bonsai/start_bonsai_8gb.bat`

### 4.5 `bonsai-12gb` (Native Windows llama.cpp, 12GB VRAM)
- **Engine**: PRISM CUDA `llama-server.exe` (native Windows)
- **Model**: `Ternary-Bonsai-2-27B-PQ2_0` (DFlash2 draft) — 1.58-bit ternary base
- **Drafter**: `Bonsai-2-27B-DFlash2-Q8_0` (fine-tuned; falls back to `Qwen3.8-27B-DFlash2-Q4_K_M`)
- **Concurrency**: 1 dedicated slot (`-np 1`)
- **Context**: 65,536 tokens (`-c 65536`), quantized KV `-ctk q4_0 -ctv q4_0`
- **Reasoning**: DeepSeek format via `delta.reasoning_content`
- **Launcher**: `scripts/bonsai/start_bonsai_12gb.bat`

### 4.6 `bonsai-16gb` (Native Windows llama.cpp, 16GB VRAM)
- **Engine**: PRISM CUDA `llama-server.exe` (native Windows)
- **Model**: `Ternary-Bonsai-2-27B-PQ2_0` (DFlash2 draft) — 1.58-bit ternary base
- **Drafter**: `Bonsai-2-27B-DFlash2-Q8_0` (fine-tuned; falls back to `Qwen3.8-27B-DFlash2-Q4_K_M`)
- **Concurrency**: 1 dedicated slot (`-np 1`)
- **Context**: 131,072 tokens (`-c 131072`), quantized KV `-ctk q4_0 -ctv q4_0`
- **Reasoning**: DeepSeek format via `delta.reasoning_content`
- **Launcher**: `scripts/bonsai/start_bonsai_16gb.bat`

---

## 4a. Hardware Matrix & Empirical Analysis: "Speculation or Context?"

When running high-parameter models on consumer VRAM (8GB, 12GB, 16GB), memory allocation is a zero-sum game between **Context Length (KV Cache)** and **Speculative Decoding (Drafter Weights)**.

Local coding agents require substantial context to ingest files, diffs, terminal outputs, and AST syntax trees without hallucinations. However, autoregressive decoding on consumer memory buses is slow (~18–25 tok/s without speculation).

All figures below are **empirically measured live on hardware via `nvidia-smi` and active engine completions**:

| GPU Tier | Target Cards | Safe Max Context | Speculation Architecture | Measured Total VRAM | Net Engine VRAM | Free Headroom | Empirical Generation Speed |
|---|---|---|---|---|---|---|---|
| **8 GB** | RTX 3070, 3060 Ti, 4060, 4060 Ti | **8,192 tokens (8K)** | **Zero-VRAM Dynamic N-Gram (`-lcd`)** | **7,686 MiB** (~7.69 GB) | 7,227 MiB | ~506 MiB | ~22–26 tok/s (syntax accelerated) |
| **12 GB** | RTX 3060 12GB, 4070, 4070 Super, 3080 12GB | **65,536 tokens (64K)** | **Lookup-Augmented DFlash2 (`-md` + `-lcd`)** | **12,074 MiB** (~12.07 GB) | 11,721 MiB | ~214–560 MiB | **~38.6 tok/s** (35–42% draft acceptance) |
| **16 GB** | RTX 4080, 4070 Ti Super, 4060 Ti 16GB, RX 7800 XT | **131,072 tokens (128K)** | **Lookup-Augmented DFlash2 (`-md` + `-lcd`)** | **13,278 MiB** (~13.28 GB) | 12,819 MiB | **>3,100 MiB (3.1 GB)** | **~53.0 tok/s** (36.5% draft acceptance) |

---

### Empirical Tier Breakdown: Speculation vs. Context Trade-off

#### 1. 8GB VRAM Tier: Context Wins (Neural Drafter Omitted)
- **The Empirical Reality**: The 27B ternary weights require ~6.70 GB of VRAM. With OS desktop overhead (~460 MiB), an 8GB card (8,192 MiB) has only ~1,000 MiB of usable headroom.
- **Why Neural Drafters Fail on 8GB**: Loading the DFlash2 speculator consumes ~500 MiB, leaving only ~500 MiB for KV cache, which collapses the context window to an unusable 2K–4K tokens.
- **The Engineering Solution**: Omit the neural drafter. Maximize the context window to **8,192 tokens (8K)** using Q4_0 KV cache (~512 MiB KV).
- **Speculation without VRAM**: Enable dynamic host-side prompt lookup (`-lcd .scratch\bonsai_dynamic_lookup.bin`). This delivers speculative n-gram speedups on code syntax, braces, variable names, and boilerplate with **0 MB of GPU memory penalty**.

#### 2. 12GB VRAM Tier: Balanced Sweet Spot (64K Context + DFlash2)
- **The Empirical Reality**: An 8K-to-64K context window is the sweet spot for single-agent coding tasks.
- **Empirical Boundary**: At **128K context**, total VRAM hits **13,231 MiB** (exceeds 12GB by ~940 MiB, spilling to system RAM).
- **The Optimal Profile**: Pinned to **65,536 tokens (64K)** with Q4_0 KV (~4.00 GB) + DFlash2 drafter (~0.50 GB). Total engine footprint is **11,721 MiB**, fitting 100% inside 12GB VRAM.
- **The Trade-off**: Sacrificing DFlash2 would only yield ~8,000 extra context tokens (from 64K to 72K), but drops decode throughput in half. Developers overwhelmingly prefer **64K context at ~38.6 tok/s** over 72K context at ~20 tok/s.

#### 3. 16GB VRAM Tier: No Compromise (Max Context + Full Neural Speculation)
- **The Empirical Reality**: On a 16GB GPU, you do **not** have to compromise between context and speculative decoding.
- **Empirical Measurement**: A full **128K context (131,072 tokens)** in Q4_0 KV (~8.00 GB) alongside the fine-tuned DFlash2 drafter (~0.50 GB) consumes **13,278 MiB total VRAM**.
- **Generous Headroom**: Leaves **>3.1 GB of unallocated VRAM**, completely eliminating OOM risk even under peak prefill activation spikes.
- **Throughput**: Delivers **52.98 tok/s decode** with an empirically verified 36.5% DFlash2 acceptance rate across 128K context.

---

## 5. Adding a New Serving Engine

To integrate a new serving backend:

1. **Map the Identifier**: Add any alias to `Backend::from_config` in `src/engine/backend.rs`.
2. **Declare Capabilities**: Define its `EngineCapabilities` (wire format, effort format, readiness probe, sentinel).
3. **Verify Wire Compatibility**: Ensure `/chat/completions` streams clean SSE frames with tool calls.
4. **Add Fixture Test**: Add a test in `tests/llama_cpp.rs` or a dedicated suite validating chat, reasoning stream, and error handling against mock endpoints.
