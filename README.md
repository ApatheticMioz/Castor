# Castor

**The Universal Cloud-to-Local Agent Bridge & MCP Microkernel.**

[![CI](https://img.shields.io/badge/CI-Passing-success?logo=githubactions&logoColor=white)](#testing--verification)
[![Rust](https://img.shields.io/badge/Rust-2024%20Edition-DEA584?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![Test Gate: 309/309](https://img.shields.io/badge/Test%20Gate-309%2F309%20Tests%20Green-success.svg)](#testing--verification)
[![Backend: Universal](https://img.shields.io/badge/Backend-Ollama%20%7C%20vLLM%20%7C%20LM%20Studio-blue.svg)](#backend--model-configuration)
[![Flagship Preset](https://img.shields.io/badge/Flagship%20Rig-Qwen3.8--27B%20%2B%20245K-purple.svg)](#flagship-reference-profile-qwen38-27b--245k-context)
[![Security](https://img.shields.io/badge/Security-137%20Containment%20Vectors-success.svg)](#zero-trust-sandboxed-file-operations)

> **Castor** is a model-agnostic serving harness and in-process agent microkernel written in Rust and exposed as a standard Model Context Protocol (MCP) server. It pairs high-reasoning cloud orchestrators (Google Antigravity, Gemini 3.8 Flash, Claude Code, Cursor) with locally-served execution models (Ollama, vLLM, LM Studio, llama.cpp) across any codebase.
> 
> **The Ultimate API Bill Cutter:** The cloud orchestrator handles high-level architecture, task decomposition, and supervisory steering. The local coworker ingests repository context, executes structural AST refactoring, runs test loops, and mutates code at **$0 token cost**. **Zero cloud token hoarding. 90%+ API cost reduction.**

---

## System Architecture

<p align="center">
  <img src="docs/assets/architecture.svg" alt="Castor Architecture &amp; MCP Delegation Workflow" width="100%" />
</p>

### The Asymmetric Division of Labor
- **Cloud Orchestrator (Lead Architect)**: Focuses strictly on architecture, formal interface specification, multimodal vision review, and supervisory steering. Never hoards raw codebase files into paid prompt context.
- **Local Coworker (Hands-on Execution @ $0)**: Ingests repositories, analyzes traces, performs AST surgery (native `ast-grep`), and verifies code locally inside the in-process Castor microkernel.
- **Zero-Turn Reactive Wait**: Long-running background dispatches yield an OS-level wait hook (`curl :18021/task/<id>/wait`). The cloud orchestrator blocks at **$0 token cost** and wakes reactively the moment the coworker completes.

---

## Consolidated MCP Interface

Castor exposes three stdio-pure MCP tools to any client:

1. **`castor_coworker`**: Primary execution interface.
   - Accepts `prompt`, `cwd`, `session_id`, `reasoning_effort` (`xhigh` | `medium` | `low`), and optional MCP `extensions`.
   - Executes with Universal 245K context and test-time deliberation.
2. **`castor_task`**: Background task management & telemetry.
   - Actions: `status`, `cancel`, `cancel_all`, `list`, `stats`, `extend_lease`.
   - Returns real-time cumulative token counts, generation throughput, cache hit rates, and financial savings.
3. **`castor_server`**: Inference engine lifecycle supervisor.
   - Actions: `status`, `start`, `stop`.
   - Manages engine boot, health canaries, wedge detection, and auto-healing.

---

## Core Capabilities

- **In-Process Microkernel & AST Surgery**: File operations and structural AST replacements (native `ast-grep`) execute in-process (<0.1 ms dispatch) without subprocess overhead.
- **In-Memory Syntax Gates**: Pre-validates modifications in-memory (TypeScript, JavaScript, Python, JSON) before committing to disk, preventing corrupted files.
- **Dual Multimodal Vision Authority**: Cloud orchestrators (Gemini 3.8 Flash) and local Qwen both support image inputs. Vision-tower CPU offload retains the complete 268K+ KV cache in GPU VRAM.
- **Multi-Provider Web & Academic Research**: Native `web_search` (with SearXNG category routing across general, science, IT), `web_fetch` (with HTML markdown and native in-memory PDF extraction with pagination), and verified OpenAlex `paper_lookup` for zero-hallucination bibliographic grounding.
- **Automated State Pruning (`castor clean`)**: Automatic retention policy over `~/.castor/` (14-day max age, 200-session count, 50MB ceiling, `.tmp_*` cleanup) with active session immunity and 24-hour startup throttling.
- **Cooperative Landing**: Dispatches reaching their turn budget conclude gracefully with mandatory deliverable synthesis under `completed_budget_exhausted` instead of arbitrary process kills.

---

## Zero-Trust Sandboxed File Operations

Castor implements a **5-layer defense-in-depth boundary** ensuring neither the coworker nor external agents can escape the workspace root:

<p align="center">
  <img src="docs/assets/security_sandbox.svg" alt="5-Layer Zero-Trust Sandbox Pipeline" width="100%" />
</p>

1. **PathEscape Normalizer**: Rejects directory traversal (`../../`), raw drive letters (`C:\`), and device namespaces (`NUL`, `CON`, `PRN`).
2. **Symlink Realpath Containment**: Resolves real canonical paths to block symlink breakouts.
3. **Workspace Root Overwrite Guard**: Protects the workspace root and parent directories from deletion or replacement.
4. **Dangerous Shell Filter**: Neutralizes destructive commands (`rm -rf /`, `format`, fork bombs, `dd`).
5. **AST In-Memory Syntax Gate**: Validates syntactical integrity before disk commit; rolls back on parse errors.

*Verified by a 137-vector automated containment suite (`src/tools/sandbox.rs`: 123 attack vectors blocked, 14 allow vectors permitted).*

---

## Quickstart

### 1-Command Setup (via npm / npx)
```bash
# Register with Claude Code & Antigravity IDE automatically
npx -y mcp-castor install --client all

# Or install globally
npm install -g mcp-castor
castor install --client all
```

### Building from Source (Rust)
```bash
git clone https://github.com/ApatheticMioz/Castor.git
cd Castor

# Build the Rust binary
cargo build --release

# Run unit and integration tests (309 tests, ~5s)
cargo test

# Register with Claude Code and Antigravity IDE
./target/release/castor install --client all
```

### Running the MCP Server
```bash
# Directly via compiled binary
./target/release/castor mcp

# Or via cross-platform Node distribution shim
node bin/castor.js mcp
```

---

## CLI Command Reference
Complementing `castor --help`:

| Command | Action | Key Options |
|---|---|---|
| `castor mcp` | Launch standard Model Context Protocol stdio server | *(default subcommand)* |
| `castor stats` | Live operational telemetry dashboard & cloud arbitrage savings | `-s <duration>`, `-d`, `-j`, `--no-color` |
| `castor install` | Auto-register server in Claude (`.claude.json`) and Antigravity (`mcp_config.json`) | `--client <all\|claude\|antigravity>` |
| `castor config` | Inspect resolved configuration hierarchy (`CASTOR_*` env > `config.json` > defaults) | *(prints effective values & sources)* |
| `castor server` | Manage serving engine lifecycle (boot, canary health, shutdown) | `<status\|start\|stop>` |
| `castor clean` | Prune stale sessions, leases, and tasks per retention policy | `--yes` (dry-run by default) |
| `castor proxy` | Run stateful SSE UTF-8 stream sanitizer proxy (`:18022` $\to$ `:18020`) | `--port`, `--engine-port` |
| `castor status` | Run zero-turn long-poll HTTP wait endpoint (`:18021`) | `--port` |
| `castor evo` | Run offline batch evaluations and view lineage DAG | `<run\|status>` |

---

## Operational Telemetry & Arbitrage Dashboard (`castor stats`)

Castor records granular telemetry locally in `~/.castor/sessions/*/events.jsonl` and `~/.castor/tasks/*.json`. The `castor stats` command parallelizes ledger ingestion across CPU cores via `std::thread::scope` (<250ms latency) and formats a modern, high-density terminal dashboard tracking token efficiency, task success rates, and actual financial savings:

```text
CASTOR OPERATIONAL TELEMETRY
Horizon: 2026-09-04 22:58 UTC -> 2026-10-05 16:54 UTC

ACTIVITY & RUNTIME
  Turns                      23,582
  Sessions                      460
  Tasks                         388  86.9% ok: 337 completed, 41 failed, 10 cancelled
  Active Compute             28.51h  4m 04s avg
  Tool Calls                 32,165  96.5% ok, 1,131 errors

TOKEN EFFICIENCY & DYNAMICS
  Total Processed             1.56B
  Prompt Tokens               1.53B
  Output Tokens              31.67M
  Reasoning Tokens           32.02M  50.3% of generation
  Reasoning Ratio             1.01x  deliberation / output
  Surgical Edit Ratio         2.66x  edits / writes

CLOUD ARBITRAGE & ENERGY (Claude Sonnet 5 Rates)
  Virtual Cloud Cost      $3,371.30
  Local Power Cost (Est)      $1.37  8.6 kWh @ $0.16/kWh, 300W
  Net Savings            +$3,369.93  99.96% net

TOOL RELIABILITY & DISTRIBUTION
  TOOL                      CALLS   SHARE  DISTRIBUTION       ERRORS  ERR RATE
  bash                     14,031   43.6%  ██████████████         19      0.1%
  read_file                 7,947   24.7%  ███████▊              633      8.0%
  edit_file                 3,990   12.4%  ███▊                  182      4.6%
  search_code               2,089    6.5%  ██                     27      1.3%
  write_file                1,501    4.7%  █▎                     49      3.3%
  list_dir                  1,048    3.3%  █                      18      1.7%
  web_fetch                   548    1.7%  ▌                      70     12.8%
  web_search                  497    1.5%  ▎                     112     22.5%
  other (14 tools)            514    1.6%  ▌                      21      4.1%
```

### Quick Usage Examples
```bash
# View all-time operational dashboard with full terminal colors
castor stats

# Restrict to recent window (e.g. last 24 hours, 7 days, 120 minutes)
castor stats -s 24h
castor stats -s 7d

# Include daily tabular breakdown of turns, tokens, and tool usage
castor stats -d

# Machine-readable JSON output for CI/CD or custom monitoring scripts
castor stats -j

# Disable ANSI coloring for plain-text logging or pipe redirection
castor stats --no-color
```

## Backend & Model Configuration

Castor connects to **any OpenAI-compatible local inference endpoint**. Configuration is loaded from `~/.castor/config.json` with environment variable overrides (`CASTOR_*`):

### 1. Ollama (Default port: 11434)
```bash
# Point to your local Ollama instance running any coding model
export CASTOR_BASE_URL="http://127.0.0.1:11434/v1"
export CASTOR_MODEL="qwen2.5-coder:32b"
```

### 2. LM Studio (Default port: 1234)
```bash
export CASTOR_BASE_URL="http://127.0.0.1:1234/v1"
export CASTOR_MODEL="qwen3.8-27b"
```

### 3. Flagship Reference Profile (Qwen3.8-27B + 245K Context)
The author's recommended high-performance setup for consumer 24 GB GPUs (NVIDIA RTX 3090 / 4090):
- **Model**: Qwen3.8-27B (hybrid dense Gated-DeltaNet + attention, 65 layers, W4A16 AutoRound).
- **Endpoint**: `http://127.0.0.1:18020/v1` (managed via `castor server start` or `scripts/wsl/start_huge.sh`).
- **Vision Offloading**: `VISION=1` + `VLLM_VISION_CPU_OFFLOAD_GB=1` keeps the vision encoder in host RAM while preserving all 268,000+ KV tokens in GPU VRAM.
- **Speculative Block Drafter**: DFlash2 1.92B non-autoregressive drafter yielding 5.33 tokens/step mean acceptance (61.9% draft acceptance rate, ~61 tok/s).
- **KVarN Tiled KV Cache**: 4-bit keys / 2-bit values per 128-token tile, sustaining a 245,760-token context ceiling.

---

## Repository Structure

```
Castor/
  Cargo.toml                # Root crate manifest
  Cargo.lock                # Deterministic dependency lockfile
  package.json              # Distribution manifest for npm shim
  bin/castor.js             # Cross-platform distribution shim
  skills/                   # Reusable SKILL.md workflow recipes
  src/
    main.rs                 # CLI entry point (clap dispatch, worker runner)
    config.rs               # Unified serde configuration loader
    platform.rs             # Cross-OS path translation and process management
    skills.rs               # agentskills.io SKILL.md indexer
    telemetry.rs            # JSONL event ledger & derived statistics
    pruner.rs               # State dir retention pruner
    mcp/                    # rmcp stdio transport, schemas, and worker
    task/                   # Task registry, long-poll HTTP wait, semaphore
    proxy/                  # Stream proxy, SSE sanitizer, repetition breaker
    engine/                 # vLLM / OpenAI client, lifecycle, health canary
    runner/                 # Autonomous agent loop, loop detection, events
    tools/                  # Sandboxed FS, AST search/replace, shell, web
    evo/                    # Offline evolutionary optimizer & lineage DAG
  evals/                    # Tier A offline replay and golden test tasks
```

---

## License & Commercial Dual-Licensing

Licensed under the **[GNU Affero General Public License v3 (AGPL-3.0)](LICENSE)** — **Castor Contributors**.

- **Open Source & Copyleft**: Free and open-source software under AGPLv3. Network use requires complete source distribution of modified versions.
- **Commercial Dual-Licensing**: Available for proprietary embedding without copyleft obligations. Inquiries: [`ApatheticMioz@gmail.com`](mailto:ApatheticMioz@gmail.com).

### Acknowledgments
- **Qwen Team (Alibaba Cloud)** for Qwen3.8-27B.
- **vLLM Project** for high-throughput LLM serving.
- **Huawei CSL** for [KVarN](https://github.com/huawei-csl/KVarN).
- **Inco AI** for the [DFlash2](https://inco.ai/blog/dflash2/) block drafter.
- **Herrington Darkholme** for [ast-grep](https://github.com/ast-grep/ast-grep).
- **syv-ai** for [HyperQwen](https://github.com/syv-ai/HyperQwen) serving baselines.
