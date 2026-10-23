# Empirical Head-to-Head Benchmark: HyperQwen vs. Bonsai

Live, side-by-side A/B evaluation conducted on an **NVIDIA GeForce RTX 3090 (24GB VRAM)** on a Windows host with 32GB RAM, benchmarking both single-agent autoregressive generation and massive 100k–200k token multi-agent interleaved workflows.

---

## 1. Executive Summary & Comparative Matrix

All tests execute on port 18020 under strict mutual exclusivity:
- **Bonsai Runtime**: Native Windows `llama.cpp` PRISM CUDA binary with fused FlashAttention and dynamic n-gram lookup (`-lcd`).
- **HyperQwen Runtime**: WSL2 Ubuntu custom vLLM 0.30.0 with AutoRound Marlin W4A16 GEMM kernels, DFlash2 speculative decoding, KVarN tiled KV compression, and CPU-offloaded Qwen-VL vision.

| Metric / Dimension | Pure Bonsai 262K ([`start_bonsai_262k.bat`](start_bonsai_262k.bat)) | Bonsai 262K DFlash2 ([`start_bonsai_dflash_262k.bat`](start_bonsai_dflash_262k.bat)) | Bonsai Max 512K Unified ([`start_bonsai_max_512k.bat`](start_bonsai_max_512k.bat)) | HyperQwen Default ([`scripts/main/start.bat`](../main/start.bat)) (245k Huge + DFlash2) | Empirical Trade-off |
|---|---|---|---|---|---|
| **Runtime Engine** | `llama.cpp` PRISM CUDA (Native Windows) | `llama.cpp` PRISM CUDA (Native Windows) | `llama.cpp` PRISM CUDA (Native Windows) | Custom vLLM 0.30.0 + DFlash2 + KVarN (WSL2 Ubuntu) | Native Windows vs WSL2 |
| **Model Weights** | **6.70 GiB** (PQ2_0 1.58-bit ternary) | 6.70 GiB base + 1.91 GiB fine-tuned draft | 6.70 GiB base + 1.91 GiB fine-tuned draft | 14.71 GiB (AutoRound W4A16) + 1.2 GiB draft | Bonsai is 54% lighter |
| **KV Cache Type** | Q8_0 (32 KiB/tok) | **Q4_0 (16 KiB/tok)** | **Q4_0 (16 KiB/tok)** | KVarN (4-bit K / 2-bit V per 128-tok tile) | Q4_0 matches KVarN efficiency |
| **Total Context / Pool** | 262,144 tokens (1 slot) | 262,144 tokens (1 slot) | **524,288 tokens** (Unified `-kvu`, 4 slots) | 245,760 tokens (`MAX_SEQS=1`) | Bonsai 512k unified pool |
| **VRAM Allocated** | **18.06 GiB** (6.25 GiB free) | **18.53 GiB** (6.04 GiB free) | **22.84 GiB** (1.73 GiB free) | 23.48 GiB (843 MiB free) | Bonsai has dynamic slot room |
| **Host RAM Architecture** | 19.8 GB free (Pageable) | 19.8 GB free (Pageable) | **7.5 GB Working Set / 22.2 GB Pageable Commit** | 10.05 GB free (**14 GB Hard-Pinned via mlock**) | Bonsai RAM is pageable for Windows apps |
| **Cold Startup Latency** | **< 2.0s** (Instant mmap) | ~5.3s (mmap base + draft) | ~5.8s (mmap base + draft) | 45 – 60s (CUDA graph compile & warmup) | Bonsai boots 10x–25x faster |
| **Single-Agent Decode Rate** | 58.7 tok/s aggregate | **72.7 tok/s** (Peak: 97.8 tok/s) | **72.7 tok/s** (Peak: 97.8 tok/s) | **112.9 tok/s** (Peak: 133.8 tok/s) | HyperQwen is 1.55x faster serial decode |
| **Resumption Turn TTFT (161k Depth)** | N/A | ~4.5s (1 slot) | **4.95s** (**0% eviction**, 52 delta toks) | **247.19s** (**100% eviction**, recomputes 161k) | **Bonsai is 50x faster on resumption** |
| **Multi-Agent Mega-Context (432k total)** | N/A | Thrashes at >1 slot | **411.29s (6.85 mins)** | **625.40s (10.42 mins)** | **Bonsai is 1.52x faster overall (saves 3.57m)** |
| **Multimodal Vision** | No (Text-only GGUF) | No (Text-only GGUF) | No (Text-only GGUF) | **Yes** (CPU-offloaded vision tower, 0 VRAM penalty) | HyperQwen has vision capability |

---

## 2. Real Mega-Context Multi-Agent Empirical Benchmark (161k + 55k Tokens)

To evaluate real production multi-agent performance without synthetic toy prompts, an identical 4-turn workload was dispatched against both architectures using live Castor microkernel source code (`src/tools/sandbox.rs`, `src/task/registry.rs`, `src/config.rs`, `src/mcp/mod.rs`, `evals/dgi/dataset.json`):
- **Phase 1 (Lead Architect Turn 1)**: Ingests **161,015 tokens** of live repository code.
- **Phase 2 (Subagent Turn 1)**: Ingests a divergent **55,166-token** evaluation dataset.
- **Phase 3 (Lead Architect Turn 2 - Resumption)**: Returns control to Lead Architect back at **161,063 tokens** depth.
- **Phase 4 (Subagent Turn 2 - Resumption)**: Returns control to Subagent back at **55,198 tokens** depth.

### Empirical Turn-by-Turn Telemetry

| Benchmark Phase | Context Depth | Bonsai Max 512K Unified ([`start_bonsai_max_512k.bat`](start_bonsai_max_512k.bat)) | HyperQwen Huge ([`scripts/main/start.bat`](../main/start.bat)) | Observed Cache State |
|---|---|---|---|---|
| **Phase 1: Agent 1 T1** | 161,015 tokens | 308.91s (~530 tok/s prefill) | **248.01s** (~650 tok/s prefill) | Cold prefill |
| **Phase 2: Subagent 2 T1** | 55,166 tokens | **64.73s** (~888 tok/s prefill) | 65.39s (~850 tok/s prefill) | Divergent prefill |
| **Phase 3: Agent 1 T2** | 161,063 tokens | **4.95s** (Evaluated 52 delta tokens in 407 ms) | **247.19s** (Full prompt recomputation) | **Bonsai: 0% eviction / HyperQwen: 100% eviction** |
| **Phase 4: Subagent 2 T2** | 55,198 tokens | **32.70s** (Evaluated 36 delta tokens in 309 ms) | **64.82s** (Full prompt recomputation) | **Bonsai: 0% eviction / HyperQwen: 100% eviction** |
| **Total Session Wall Time** | **432,442 prompt tokens** | **411.29s (6.85 minutes)** | **625.40s (10.42 minutes)** | **Bonsai finishes 3.57 minutes faster (1.52x speedup)** |

*Underlying JSON ledgers: [`scratch/mega_bonsai_mega_512k_result.json`](../../scratch/mega_bonsai_mega_512k_result.json) and [`scratch/mega_hyperqwen_huge_result.json`](../../scratch/mega_hyperqwen_huge_result.json).*

---

## 3. Host RAM Architecture: Pageable Commit vs. Pinned Memory

A primary architectural distinction between native Windows `llama.cpp` and WSL2 `vLLM` is how host RAM is registered and managed by the operating system kernel:

### 3.1 Bonsai: Pageable Virtual Commit (Windows First)
Measured live via the Windows API (`GetProcessMemoryInfo`) under active 512K load:
- **Physical Working Set**: **7,687 MB (~7.51 GB)**
- **Private Virtual Commit (`PagefileUsage`)**: **22,722 MB (~22.19 GB)**

`llama-server.exe` allocates host memory tiers (`--cache-ram 16384`) using standard user-mode virtual allocations (`VirtualAlloc` / `malloc` with `MEM_COMMIT`). These pages are **not pinned** (`VirtualLock` is not applied). 

**The Operating Invariant:**
- Windows and foreground applications (`cargo build`, IDEs, browsers) have primary claim over physical RAM.
- When physical memory pressure arises, the Windows Memory Manager transparently trims `llama-server.exe`'s working set and pages inactive context blocks out to the NVMe-backed pagefile.
- The host never experiences hard out-of-memory lockups from memory reservation.

### 3.2 HyperQwen (WSL2): Pinned Host Memory (`mlock`)
In `vLLM`, high-speed DMA transfers across the PCIe bus require page-locked buffers:
- When running with `VLLM_WSL2_ENABLE_PIN_MEMORY=1`, `cudaHostAlloc` locks **~14 GB of host RAM**.
- The Linux kernel marks these pages unpageable (`mlock`), preventing the Windows host hypervisor from reclaiming them for other applications.
- If total host memory commit exceeds physical limits, WSL2 triggers PCIe host-backing stalls and `dxgkio_escape` driver deadlocks.

---

## 4. Deep Technical Breakdown & Optimization Invariants

### 4.1 Hybrid GDN-Transformer Core
Both HyperQwen and Bonsai execute the identical fundamental topology:
- **48 recurrent Gated Delta Net (GDN) linear attention layers**
- **16 sliding-window / full self-attention layers**

Bonsai (Ternary-Bonsai-2-27B) quantizes this structure to 1.58-bit ternary weights ($\{-1, 0, +1\}$), reducing base model weight consumption from 14.71 GiB down to 6.70 GiB.

### 4.2 Why HyperQwen Thrashes on Multi-Agent Workloads
On a 24 GB GPU, HyperQwen stores KV caches in unquantized 16-bit format (`float16`/`bfloat16` = 2.0 bytes/token per layer).
1. **Physical Cache Limit**: After allocating 14.71 GiB for base weights, 1.2 GiB for the drafter, and static CUDA graphs, vLLM retains enough free VRAM for only **~65,000–70,000 KV cache tokens**.
2. **The Eviction Cascade**:
   - A single deep prompt of 161,000 tokens exceeds the physical pool ceiling, forcing chunked prefill to evict early blocks.
   - When a subagent runs a 55,000-token turn, it overwrites the remaining cache space.
   - Returning to the Lead Architect requires a full recomputation of 161,000 tokens, introducing a **247-second blocking penalty** per resumption turn.

### 4.3 Why Bonsai 512K Unified Prevents Thrashing
1. **Q4_0 Quantized KV (16 KiB/token)**: Compresses K and V tensors to 4-bit, cutting KV memory requirements across attention layers by 75% compared to 16-bit.
2. **Unified Dynamic Pool (`-c 524288 -kvu -np 4`)**: Allocates a single shared pool of 524,288 tokens across 4 execution slots. Slots dynamically claim tokens from the shared pool rather than enforcing rigid per-slot partitions.
3. **Simultaneous Resident Context**: Both Agent 1 (161k) and Subagent 2 (55k) fit concurrently in memory (216,181 total resident tokens). Turn 3 resumes in **4.95 seconds** because the engine evaluates only the 52-token prompt delta.

---

## 5. Production Launchers

Choose the launcher that matches the target workflow profile:

1. **[`start_bonsai_max_512k.bat`](start_bonsai_max_512k.bat)** (Recommended for Heavy Multi-Agent Swarms & Mega-Context):
   - Boots a 524,288-token unified hardware pool (`-kvu -np 4`) in 22.84 GiB VRAM with 16GB pageable host RAM cache tier (`--cache-ram 16384`).
   - Eliminates multi-agent KV cache thrashing up to 512k aggregate tokens. Resumption turns execute in seconds.
2. **[`start_bonsai_dflash_262k.bat`](start_bonsai_dflash_262k.bat)** (Recommended for High-Headroom Single-Agent Bonsai):
   - Boots 262,144 context (1 slot) with fine-tuned DFlash2 + dynamic lookup (`-lcd`) + Q4_0 KV cache.
   - Delivers **72.7 tok/s** aggregate decode (bursts to **97.8 tok/s**) with **6.04 GiB of free VRAM headroom** and **19.8 GB host RAM free**.
3. **[`start_bonsai_multiagent_262k.bat`](start_bonsai_multiagent_262k.bat)** (Recommended for Dual-Agent Partitioned Workloads):
   - Boots two independent 131,072-token hardware KV slots (`-np 2 -c 262144`) in 18.53 GiB VRAM.
4. **[`scripts/main/start.bat`](../main/start.bat)** (Recommended for Maximum Serial Decode & Vision):
   - Boots HyperQwen Huge (245,760 context, **112.9 tok/s**, multimodal vision tower, 23.48 GiB VRAM). Ideal for single-agent chat and visual screenshot inspection.
