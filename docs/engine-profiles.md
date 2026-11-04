# Serving Engine Profiles & Capability Model

Castor is an **engine-agnostic autonomous microkernel**. The underlying model (Bonsai, Qwen, DeepSeek, or any open-weights / proprietary model) and the serving engine (`llama.cpp`, `vLLM`, `SGLang`, `Ollama`, `LM Studio`, or a generic OpenAI-compatible API) are **pluggable profiles**, not system identity.

The microkernel's core invariants—zero-trust workspace containment, structural AST surgery, task registry, telemetry ledger, and evolutionary optimizer—remain invariant regardless of the serving engine. Only the **Serving Engine Profile** adapts to the target engine's wire protocol and hardware topology.

---

## 1. Architecture Overview

```
Lead Architect (Cloud: Claude / Gemini / Antigravity)
        │
        │  MCP over stdio (castor_coworker, castor_task, castor_server)
        ▼
Castor Microkernel (Rust)
        ├── Zero-Trust Security Sandbox & AST Surgery
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

An engine profile is declared in `~/.castor/config.json` (or via `CASTOR_*` environment variables; see the inventory in [`AGENTS.md`](../AGENTS.md) §2.5). The microkernel resolves a profile through **dynamic capability discovery** rather than a hard-coded profile table: the running engine is fingerprinted at boot, and the model / context window are auto-adopted from it when you leave them unset.

```json
{
  "base_url": "http://127.0.0.1:18020/v1",
  "max_concurrent_tasks": 4,
  "launch_command": "bash scripts/wsl/start_swarm.sh",
  "stop_command": "bash scripts/wsl/stop_server.sh",
  "boot_timeout_secs": 180
}
```

The minimal profile above is the **Swarm baseline**: point it at the engine endpoint and let discovery fill in the rest. `model` and `max_context` are deliberately omitted so they are auto-resolved (below).

### Dynamic Capability Discovery

At boot, `EngineProfile::discover` (`src/engine/backend.rs`) probes the running endpoint with three standard OpenAI-compatible calls and derives the backend, model, and context window from the evidence:

| Probe | Endpoint | What it yields |
|---|---|---|
| Model list | `GET {base}/models` (`/v1/models`) | served model ids + per-model `max_model_len`; `owned_by` fingerprint |
| Health | `GET /health` | llama.cpp returns `{"status":"ok"}`; vLLM returns an empty 200 at the server root |
| Version | `GET /version` | vLLM reports a JSON `version` (a strong vLLM fingerprint) |

**Backend identification** (most specific wins):

- `vllm` — `/v1/models` reports `owned_by: "vllm"` on any entry, **or** `/version` returns a JSON `version`.
- `llama.cpp` — `/health` returns `{"status":"ok"}`, **or** `/v1/models` reports `owned_by: "llama.cpp"`.
- `generic` — a healthy model list with no engine-specific fingerprint.
- fallback — when no live fingerprint matches, the `engine_type` setting selects the capability profile.

**Model auto-binding**: when `model` is unset (or `"auto"`), the profile binds to the *single* model reported by `/v1/models`. If the engine serves zero models, discovery fails; if it serves multiple, discovery errors and asks you to pin `CASTOR_MODEL` to one of them. An explicitly set model is bound as-is and validated against the engine by the boot canary.

**Context auto-adoption**: when `max_context` is unset, the profile adopts the engine-reported `max_model_len` from `/v1/models`. Set it explicitly to override the engine-reported value (e.g. to cap a 245k window down to a smaller budget).

### Key Configuration Keys

- `engine_type`: aliases the capability profile — `llama.cpp` / `llama` / `bonsai` → `LlamaCpp`; `vllm` / `hyperqwen` → `Vllm`; `generic` / `openai` / `sglang` / `ollama` / `lmstudio` → `Generic`. (Only used as the fallback when no live fingerprint matches.)
- `base_url`: HTTP(S) endpoint URL of the serving engine (default `http://127.0.0.1:18020/v1`).
- `model`: target model identifier; when unset, auto-bound to the engine's single served model.
- `max_context`: context window in tokens; when unset, auto-adopted from the engine's `max_model_len`.
- `max_concurrent_tasks`: concurrency slots for simultaneous worker execution (file-backed lease semaphore); default `4` (the Swarm baseline `MAX_SEQS=4`).
- `launch_command` / `stop_command`: managed startup/teardown scripts (default: the Swarm launcher for the active platform).

---

## 4. The Swarm Baseline & User-Defined Profiles

The microkernel ships **no compiled-in engine profiles** — its only built-in default is the **Swarm baseline** (the `SWARM_*` constants in `src/config.rs`). Every other named profile (`turbo`, `bonsai`, …) is **user-defined**: declared in the file's `profiles` map and selected by `CASTOR_PROFILE` (or the file's `profile` key). Selecting a name the file does not define is an error.

### 4.1 Swarm Baseline (the built-in default)

The out-of-the-box default. A bare install with no `config.json` boots and talks to this:

- **Engine**: vLLM OpenAI-compatible server (WSL2 / Linux) — `base_url: http://127.0.0.1:18020/v1`.
- **Concurrency**: `4` slots — matches the launcher's `MAX_SEQS=4`.
- **Context**: **150k** (`max_len: 150000`) — auto-adopted from the engine's `max_model_len` when `max_context` is unset.
- **Model**: auto-bound to the single served model (Qwen3.8-27B on the Swarm launcher) when `model` is unset.
- **Launcher**: `bash scripts/wsl/start_swarm.sh` (`MAX_SEQS=4`, `CTX=long`, MTP+PTH_FA speculative decoding, prefix caching).
- **Reasoning**: `chat_template_kwargs: {"reasoning_effort": effort}` (vLLM capability profile).

Each of the 4 concurrent workers additionally token-budgets the shared 150k KV pool (a deterministic per-lease prefix reserve clamped to 4k–8k, plus the task's own prompt/history tokens) so the 4 streams never overcommit the aggregate footprint — further dispatches FIFO-queue on the slot semaphore.

### 4.2 User-Defined JSON Profiles

Named profiles are ordinary entries in the file's `profiles` map. Each is a partial profile — it overrides only the keys it sets and **inherits the Swarm baseline for everything it omits** (base URL, engine type, concurrency, context). Precedence: `CASTOR_*` env vars > the file's top-level keys > the selected profile > the Swarm baseline.

```json
{
  "profile": "turbo",
  "profiles": {
    "turbo": {
      "engine_type": "vllm",
      "max_context": 245760,
      "max_concurrent_tasks": 2,
      "launch_command": "bash scripts/wsl/start_turbo.sh"
    },
    "bonsai": {
      "engine_type": "llama.cpp",
      "model": "Ternary-Bonsai-2-27B-PQ2_0",
      "max_context": 524288,
      "max_concurrent_tasks": 1,
      "launch_command": "cmd /c scripts\\bonsai\\start_bonsai_24gb.bat"
    }
  }
}
```

- **`turbo`** — peak single-user burst: `MAX_SEQS=2`, `max_context: 245760` (DFlash2 + KVarN 245k context). Launches via `scripts/wsl/start_turbo.sh`.
- **`bonsai`** — native Windows `llama.cpp`: a 1-slot, 524,288-token unified KV pool, model pinned explicitly. Launches via the `scripts/bonsai/` launchers.

Because profiles are partial, a profile that sets only `max_context` (e.g. `{"max_context": 245760}`) inherits the Swarm baseline's `base_url`, concurrency, and launcher for all other keys.

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
