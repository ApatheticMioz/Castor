<div align="center">

<img src="https://raw.githubusercontent.com/ApatheticMioz/Castor/main/docs/assets/logo.svg" alt="Castor Logo" width="128" height="128">

# Castor

**The bridge between the cloud frontier and the local frontier.**

Pair high-reasoning cloud orchestrators (Claude Code, Google Antigravity, Cursor) with locally-served execution models at **$0 cloud token cost**.

<p>
  <a href=".github/workflows"><img alt="CI" src="https://img.shields.io/badge/CI-Passing-1d4ed8?logo=githubactions&logoColor=white"></a>
  <a href="https://www.npmjs.com/package/mcp-castor"><img alt="npm" src="https://img.shields.io/badge/npm-mcp--castor-d97706?logo=npm&logoColor=white"></a>
  <a href="https://www.rust-lang.org/"><img alt="Rust" src="https://img.shields.io/badge/Rust-2024_Edition-1d4ed8?logo=rust&logoColor=white"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/License-AGPL--3.0-161e2e"></a>
  <a href="#building--verification"><img alt="Test Gate" src="https://img.shields.io/badge/Test_Gate-Passing-1d4ed8"></a>
</p>

<p>
  <a href="https://raw.githubusercontent.com/ApatheticMioz/Castor/main/docs/assets/demo.mp4">
    <img src="https://raw.githubusercontent.com/ApatheticMioz/Castor/main/docs/assets/demo.webp"
         alt="Castor in action: cloud architect dispatches, local engine executes"
         width="100%">
  </a>
  <br>
  <sub>
    <b>Watch:</b> Cloud orchestrator dispatches a prompt → Castor microkernel drives the local serving
    engine through the in-tree tool loop → concise deliverable returns to the cloud.
    <a href="https://raw.githubusercontent.com/ApatheticMioz/Castor/main/docs/assets/demo.mp4">1080p60 MP4</a>.
  </sub>
</p>

</div>

---

## Quickstart

### 1. Install & Register

Register Castor with every detected MCP client in one command:

```bash
# One-shot, no global install required:
npx -y mcp-castor install --client all

# …or install globally first:
npm install -g mcp-castor
castor install --client all
```

Prefer manual wiring? Drop this into your client's MCP config (`claude_desktop_config.json`, `~/.gemini/config/mcp_config.json`, or Cursor MCP settings):

```json
{
  "mcpServers": {
    "castor": {
      "command": "npx",
      "args": ["-y", "mcp-castor", "mcp"]
    }
  }
}
```

### 2. Connect Your Local Engine

Castor auto-discovers any OpenAI-compatible endpoint at `http://127.0.0.1:18020/v1` by probing
`/v1/models` to detect the model name, context window (`max_model_len`), and engine backend
(vLLM, llama.cpp, LM Studio, SGLang, Ollama) automatically. No config file or environment variables needed.

For custom endpoints or non-default ports, set `CASTOR_BASE_URL`:

```bash
# LM Studio (port 1234)
export CASTOR_BASE_URL="http://127.0.0.1:1234/v1"

# vLLM (custom port)
export CASTOR_BASE_URL="http://127.0.0.1:8000/v1"

# SGLang / Ollama / any other OpenAI-compatible /v1
export CASTOR_BASE_URL="http://127.0.0.1:<port>/v1"
```

See [docs/llama-cpp.md](docs/llama-cpp.md) for managed `llama-server` startup/shutdown and
Windows/Linux examples. Configuration persists to `~/.castor/config.json`; environment variables always win.

### 3. Pair Program at $0

Prompt your cloud agent:

> *"Refactor auth middleware to use JWT and verify with `cargo test` using castor."*

The cloud architect plans. Castor executes the edit-and-test loop against the local model
in-tree at **$0 cloud token cost**. Only concise, verified deliverables return to the cloud.

---

## Why Castor?

| Dimension | Pure Cloud Agent | Local Model Alone | **Castor Dual-Agent** |
|---|---|---|---|
| **Architectural reasoning** | Top-tier | Limited by model size & context | **Cloud frontier** (Claude, Gemini, Antigravity, Cursor) |
| **Hands-on tool loop** | Every file read, grep, and log re-billed at cloud rates | Runs locally with limited planning quality | **Local serving engine ($0 cloud tokens)** |
| **Cost per iteration** | Cloud API billed per token every turn | ~$0 (weak planning) | **~$0 cloud cost** (only verified deliverables cross the wire) |
| **Context window load** | Grows rapidly with raw terminal dumps & logs | Same | Isolated entirely to local engine context |
| **Loop latency** | Network round-trip per tool invocation | Local | **Local in-process tool dispatch** |
| **Safety & containment** | Vendor-dependent | DIY | **Zero-trust workspace containment pipeline** |

**The core insight:** In a cloud-only agent, every tool output (reading 20 files, running `cargo test`, grepping the codebase) is re-sent and re-billed at cloud rates on every subsequent turn. Castor inverts this: the hands-on tool loop executes in-tree against the local serving engine at **$0 cloud cost**, returning only verified deliverables to the cloud.

---

## System Architecture

<img src="https://raw.githubusercontent.com/ApatheticMioz/Castor/main/docs/assets/architecture.svg"
     alt="Castor system architecture" width="100%">

**Flow:**

1. **Cloud Architect** (Claude Code, Google Antigravity, Cursor) plans and dispatches a
   prompt via the MCP tool `castor_coworker(prompt, cwd)`.
2. **stdio MCP transport** carries the request to the **Castor Microkernel**, an in-process
   Rust runtime providing an 8-tool core and zero-trust workspace containment.
3. **Castor → Local Serving Engine** over HTTP (`:18020`). The engine (vLLM, llama.cpp,
   LM Studio, SGLang, Ollama) executes the tool loop (reading, editing, grepping, running tests)
   at **$0 cloud token cost**.
4. The **concise, verified deliverable** returns to the cloud architect for final synthesis.

For long tasks, the microkernel yields a durable task ID and a long-poll wait hook on
`:18021` so the cloud supervisor sleeps at $0 and wakes the millisecond execution
completes (see § *Zero-Turn Reactive Lifecycle* below).

---

## Core Innovations

### 1. DGI (Decomposition Granularity Index)

A **pre-execution firewall** that stops compound "mega-prompt" thrashing before it burns
compute or leases a slot. A **1-forward-pass logit probe** (sub-25 ms on vLLM via `guided_choice`;
scales with decode speed on llama.cpp) against the serving engine classifies the
dispatch prompt; the decision rule is:

```
delta_z = logP(OVERLOADED) − logP(ADMIT)
REJECT  if delta_z > 1.5
```

Near-tie margins are routed to an advisory review rather than silent admission.
**Warm sessions** (continuations of an already-warm context) bypass the probe entirely with
zero overhead. The probe is **engine-agnostic**: vLLM uses a `guided_choice` single-token
completion; llama.cpp uses a schema-constrained JSON chat decode. Same decision rule,
DRY request-builder and verdict-decoder core in [`src/mcp/dgi.rs`](src/mcp/dgi.rs).

### 2. Zero-Turn Reactive Lifecycle

Tasks exceeding the sync window (default **15 s**, tunable via `CASTOR_SYNC_TIMEOUT_SECS`)
yield a durable task ID and a long-poll HTTP wait hook:

```bash
curl -N "http://127.0.0.1:18021/task/<task-id>/wait"
```

The cloud supervisor blocks on that URL at **$0 token cost** and wakes reactively the
millisecond execution completes without polling loops or prompt re-sends.
Implemented in [`src/task/`](src/task/).

### 3. Token-Budget Semaphore & Cooperative Yielding

Zero-lock daemon. Slot admission is a **two-phase atomic `O_EXCL` lease**
(claim → ownership-check release), with **zombie-aware Linux PID liveness**
(via `/proc/{pid}/status`, `State: Z` is treated as dead so stale leases are reclaimed).

The admission budget is expressed in **tokens against the aggregate KV pool**
(`pool_capacity_tokens`). When pressure builds, the **cooperative yield gate**
(`pool_yield_gate`) lets younger sessions voluntarily yield to older ones,
preventing GPU VRAM thrashing under concurrency spikes.
See [`src/task/semaphore.rs`](src/task/semaphore.rs).

### 4. Stream Proxy & Sanitizer

A real-time SSE proxy that monitors live model output on `:18022` for degenerate stream patterns:

- **UTF-8 packet-fragment reassembly**: Guarantees safe multi-byte and emoji boundaries across TCP chunks.
- **Repetition circuit breaker**: Halts mid-stream degenerate loops (identical content repeating within a rolling window) before context exhaustion.
- **Payload guards**: Filters malformed tool and image payloads before they reach the parser.

See [`src/proxy/`](src/proxy/).

### 5. Evolutionary Optimizer (Evo)

Closed-loop code mutation guided by an **append-only, cycle-proof lineage DAG**
(`src/evo/`). Each mutation is gated on a **deterministic test fitness** evaluation;
a **stagnation watchdog** (`src/evo/watchdog.rs`) inspects the DAG head and flags
plateaus so the loop can be reset before drift accumulates.

### 6. Lean 8-Tool Core

Eight core tools cover all workspace execution without bloat:

```
read_file · write_file · edit_file · list_dir · search_code · bash · web_search · web_fetch
```

Git operations (`git status`, `diff`, `log`, `commit`) and ad-hoc Python run natively through `bash`. Two optional feature gates extend capabilities for specialized workflows:

- `ast`: Structural `ast_search` and `ast_replace` via ast-grep with automatic syntax rollback.
- `paper`: Academic paper lookup (`paper_lookup`) against OpenAlex for verified research metadata and open-access PDFs.

### 7. Lazy Skills Engine

Compatible with the [`agentskills.io`](https://agentskills.io) `SKILL.md` standard. Skills are
indexed in the system prompt with just their name and summary, and loaded on demand only when
the active task requires them. This preserves the **Turn-1 KV prefix cache** across tasks in the
same workspace so repeat prompts avoid full re-prefill penalties.

---

## Workspace Containment

<img src="https://raw.githubusercontent.com/ApatheticMioz/Castor/main/docs/assets/security_sandbox.svg"
     alt="Castor workspace containment" width="100%">

Castor enforces zero-trust containment at every tool boundary:

| Stage | Mechanism | What it blocks |
|---|---|---|
| **1. Path normalizer** | Canonical path resolution & device namespace neutralization | `../../etc/passwd`, `C:\`, `NUL`, `CON` |
| **2. Realpath anchor** | Symlink traversal audit & workspace boundary guard (`refuse_out_of_tree`) | Symlink escapes, out-of-workspace writes, system directories |
| **3. Command filter** | Shell AST inspection & signature blocks (`shell.rs`) | `rm -rf /`, `format`, `dd`, fork bombs, `mkfs`, shell injection bypasses |
| **4. AST syntax gate** | Structural syntax parser validation before disk commit | In-memory rollback on syntax error across Rust, TS, JS, Python, JSON |

File edits in `fs.rs` use atomic fsync writes via `NamedTempFile` to prevent torn states.
Structural mutations via `ast_replace` validate syntax in-memory before writing to disk,
atomically rolling back if parsing fails.

---

## MCP Tool Surface

Castor registers three stdio MCP tools:

| MCP Tool | Purpose | Actions / Parameters |
|---|---|---|
| **`castor_coworker`** | Hands-on execution & peer programming | `prompt`, `cwd` — dispatches into the sandboxed 8-tool runtime + feature-gated `ast` / `paper` |
| **`castor_task`** | Background task control & telemetry | `status` · `cancel` · `cancel_all` · `list` · `stats` · `extend_lease` |
| **`castor_server`** | Local serving-engine supervisor | `status` · `start` · `stop` — managed engine lifecycle with health canaries and auto-healing |

**Tool Reference:**

| Tool | Feature Gate | Capability |
|---|---|---|
| `read_file` | core | Read files, PDFs (in-memory text extraction), and images (PNG/JPG/WebP/GIF/BMP) |
| `write_file` | core | Create or overwrite files within the workspace root |
| `edit_file` | core | Exact-substring edit with uniqueness checks and atomic tempfile replacement |
| `list_dir` | core | Directory listing with optional depth control |
| `search_code` | core | Deterministic path-filtered text and regex search |
| `bash` | core | Sandboxed shell with AST command filter; native `git` and `python3` |
| `web_search` | core | Web meta-search (SearXNG primary, Brave Search fallback) |
| `web_fetch` | core | Fetch and convert HTML, PDF, or plaintext into clean Markdown |
| `ast_search` | `ast` | Structural AST search via ast-grep (metavariables `$VAR`, `$$$ARGS`) |
| `ast_replace` | `ast` | Structural AST refactoring with automatic rollback on syntax error |
| `paper_lookup` | `paper` | Academic metadata via OpenAlex (titles, DOIs, affiliations, open-access PDFs) |

---

## Ecosystem Matrix

**Supported Cloud Clients**

| Client | Registration |
|---|---|
| **Claude Code** | `castor install --client claude` |
| **Google Antigravity** | `castor install --client antigravity` (writes `~/.gemini/config/mcp_config.json`) |
| **Claude Desktop** | `claude_desktop_config.json` (manual or via installer) |
| **Cursor** | Cursor MCP configuration |
| **Any MCP client** | stdio MCP — standard JSON server configuration |

**Supported Serving Engines**

| Engine | Default Port | Notes |
|---|---|---|
| **vLLM** | 18020 | Auto-detected via `/v1/models` (`owned_by: "vllm"`); `guided_choice` DGI probe |
| **llama.cpp** (`llama-server`) | 18020 | Auto-detected via `/v1/models`; `--jinja` tool calling; managed start/stop via `castor server` |
| **LM Studio** | 1234 | Configure via `CASTOR_BASE_URL` |
| **SGLang** | 30000 | Configure via `CASTOR_BASE_URL` |
| **Ollama** | 11434 | Configure via `CASTOR_BASE_URL` |
| **Any OpenAI-compatible endpoint** | any | Configure via `CASTOR_BASE_URL` |

Auto-detection queries `/v1/models` (model name, `max_model_len`) and server fingerprints (`/health`, `/version`, `owned_by`) to apply optimal engine profiles without manual configuration.

---

## CLI Reference

```bash
# Model Context Protocol stdio server (default entry point)
castor mcp

# Register with MCP clients (claude | antigravity | all)
castor install --client all

# Manage local serving engine lifecycle (status | start | stop)
castor server start

# Operational telemetry dashboard (JSONL event ledger → live TUI)
castor stats [-s 24h] [-d] [-j]

# Inspect effective configuration (env > config.json > defaults)
castor config

# Clean stale sessions, leases, and tasks per retention policy
castor clean [--yes]
```

Additional subcommands include `castor status` (zero-turn HTTP wait server on `:18021`) and `castor proxy` (SSE stream proxy on `:18022`). See [`src/cli.rs`](src/cli.rs) for full CLI definitions.

---

## Building & Verification

```bash
# Clone repository
git clone https://github.com/ApatheticMioz/Castor.git
cd Castor

# Build release binary
cargo build --release

# Offline test gate (deterministic unit and integration harnesses)
cargo test --all-features

# Strict Clippy gate (zero warnings enforced)
cargo clippy --all-targets --all-features -- -D warnings
```

---

## License & Inquiries

Licensed under the **[GNU Affero General Public License v3.0 (AGPL-3.0)](LICENSE)**.

- **Open Source**: Free software under AGPLv3. Network-deployed modifications must make source code available.
- **Commercial Dual-Licensing**: Available for proprietary embedding without copyleft obligations. Inquiries: [`ApatheticMioz@gmail.com`](mailto:ApatheticMioz@gmail.com).
