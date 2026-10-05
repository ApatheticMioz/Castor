# Changelog — Castor

All notable changes to the **Castor** Universal 245K Agent Microkernel & MCP Server are documented in this file.

The project adheres to **[Semantic Versioning](https://semver.org/)**:
- **`1.0.0`** (2026-10-03) — Full Rust Microkernel Rewrite, Native AST-Grep, rmcp 3.5, and Complete v1.0.0 Cutover.
- **Historical CalVer Releases**: `2026.3.0`, `2026.2.0`, `2026.1.0`.

---

## [1.0.2] — 2026-10-05

### 🛡️ Landlock LSM Confinement & Security Hardening (Issue #6, #17)
- **Landlock LSM Integration**: Kernel-enforced filesystem confinement on Linux/WSL using standard `landlock = "0.4"` crate in `src/tools/shell.rs`. Confines child bash processes to workspace root and `/tmp` (RW) while granting read-only access to system paths and `~/.castor`.
- **Character Device & Toolchain Hardening**: Granted read-write access to `/dev` to ensure `/dev/null`, `/dev/zero`, and `/dev/urandom` support `O_RDWR` writes (unblocking `git` and shell redirections). Granted read-only access to user toolchains (`~/.cargo`, `~/.rustup`, `~/.local`) and prepended `$HOME/.cargo/bin:$HOME/.local/bin` to `PATH`.
- **Symmetric Multi-Root Scoping**: Parity between shell confinement and filesystem tools via `SandboxPolicy::for_workspace`.
- **28 Tree-Sitter Languages**: Expanded AST tooling in `src/tools/ast.rs` with all 28 language features in `ast-grep-language`.
- **AST Pass-By-Reference**: Optimized AST tree-sitter recursive traversal to borrow `&Node` references, eliminating unnecessary node copies.

### ⚡ DGI 1-Forward Pass Logit Probe (Issue #16)
- **System-1 Constrained Token Probe**: Replaced 711 LOC of brittle regexes and heuristics with a single-pass constrained logit probe using `guided_choice: ["ADMIT", "OVERLOADED"]`, `max_tokens: 1`, temperature 0.0, and prefilled thought suppression. Sub-30ms decode with zero autoregressive reasoning loops.
- **Fail-Fast Gateway Rejection**: True monolithic dispatches trigger immediate `DecompositionGateRejected` at the MCP gateway.
- **Loud Offline Fallback**: Degrades gracefully to advisory `Review` with explicit warning logs when serving engine is unreachable.
- **Expanded Evaluation Benchmark**: Expanded test corpus to 401 cross-domain prompts with labeled ground truth.

### 🧠 Reasoning Effort Direct Passthrough (Issue #3)
- **Direct chat_template_kwargs**: Direct wiring of `chat_template_kwargs: {"reasoning_effort": effort}` to vLLM, eliminating dual-payload abstractions.
- **Reasoning Token Telemetry**: Extracted reasoning token consumption from OpenAI-compatible provider responses into the session event stream.

### ⏱️ Runner Probe Budget Governor (Issue #17)
- **Proactive Loop Breaker**: Consecutive probe warnings trigger at 4 non-mutating bash commands, with an explicit bypass exemption for sanctioned `.scratch/` scripts.
- **Tool Output Telemetry**: Recorded full tool execution outcomes in event stream for real-time observability.

### 🔎 Default Search Provider
- **SearXNG on :8888**: Configured local SearXNG at `http://127.0.0.1:8888` as primary default web search provider, with seamless fallback to Brave and DuckDuckGo.

### 📈 Telemetry Windowed Queries (Issue #18)
- **`--since <duration>`**: Time-bound `castor stats` to a rolling window ending at now (e.g. `--since 24h`, `--since 7d`, `--since 30m`). Duration parsing uses standard `humantime` (2.1).
- **`--by-day`**: Aggregate session events by UTC civil date (`YYYY-MM-DD`) using `chrono`. The daily breakdown covers turns, prompt / completion / reasoning tokens, tool calls / errors, attributed session count, and attributed session duration.
- **Card & JSON surfaces**: The `format_stats_card` operator card gains a `📅 DAILY BREAKDOWN (UTC)` table when `--by-day` is active; the `--json` surface gains a top-level `daily` object.
- **Dual Snake/CamelCase Serde**: Robust aggregation support for both canonical `snake_case` and `camelCase` token metric fields.

### 🚀 Lean Build & Dependency Optimization
- **Stripped Unused Dependencies**: Removed unreferenced `shlex = "1.3"` from `Cargo.toml`. Verified 0 unused dependencies via `cargo-machete`.
- **Release Profile Tuning**: Configured `[profile.release]` with `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, and `strip = true` for minimal binary footprint while retaining C-ABI unwinding compatibility with native tree-sitter grammars.
- **Documentation Link Integrity**: Resolved all broken intra-doc link references (`\[DONE\]`, `Self::final_text`, `Self::web_search`, `Self::fetch_docs`) under `RUSTDOCFLAGS="-D warnings"`.
- **Package Manifest Metadata**: Added canonical `repository` and `homepage` URLs to `Cargo.toml` package definition.
- **Codebase-Wide Canonical Formatting**: Applied standard `rustfmt` repo-wide to ensure clean RFC formatting across all modules.
- **Multi-Tool Verification Matrix**: Successfully passed 24 distinct Rust/Cargo verification suites (`cargo test`, `clippy -D warnings`, `cargo-audit`, `cargo-deny`, `cargo-bloat`, `cargo package`).

---

## [1.0.0] — 2026-10-03

### 🦀 Complete Rust Rewrite & v1.0.0 Full Cutover
- **Native Rust Microkernel**: Completely re-engineered Castor from Node.js into a single clean, high-performance Rust crate. 18.8K LOC JS replaced by ~5.5K LOC idiomatic Rust.
- **Sub-Millisecond In-Process Dispatch**: Microkernel executes AST queries, sandbox validations, and session iterations with zero V8/Node startup overhead and minimal memory footprint.
- **Native AST Surgery via `ast-grep`**: Replaced Node `@ast-grep/napi` FFI wrapper with direct native Rust crate integration (`ast-grep-core`, `ast-grep-language`), providing rock-solid cross-platform AST matching and mutation.
- **Zero-Trust 5-Layer Sandbox (137 Vectors)**: Re-implemented the 5-layer containment boundary (PathEscape, Symlink Realpath, Root-Overwrite Guard, Destructive Shell Validator, and In-Memory Syntax Gate) natively in Rust, verified against all 137 vectors (123 blocked, 14 allowed).
- **Official MCP Rust SDK (`rmcp` 3.5)**: Pure stdio JSON-RPC transport powered by `rmcp` 3.5 with compile-time Schemars schema generation for `castor_coworker`, `castor_task`, and `castor_server`.
- **Zero-Turn Reactive Wait HTTP Server**: `axum` 0.8 long-poll status server on `:18021` (`GET /task/:id/wait`) serving waiters from disk mirror and memory with sub-millisecond wakeup on task completion.
- **Universal SSE Stream Proxy (`:18022`)**: `encoding_rs`-based streaming proxy with incremental UTF-8 reassembly, repetition circuit breaking, and mid-stream error translation.
- **Offline Evolutionary Optimizer (AVO-Inspired)**: Replaced fragile online single-turn mutations with an offline reflective batch evaluator (`castor evo`), lineage DAG, and stagnation watchdog.
- **Model-Routed Skills Indexing**: Standardized on agentskills.io format (`SKILL.md`), rendering a compact cache-friendly index into the system prompt with zero keyword-matching bloat.
- **Authoritative Test Gate**: 243 unit and integration tests passing in ~7 seconds with zero warnings on `cargo clippy --all-targets -- -D warnings`.
- **Cross-Platform npm Shim**: Lightweight Node distribution shim (`bin/castor.js`) and root `package.json` for seamless `npx -y mcp-castor` and IDE client integration on Windows (via WSL2 bridge) and Linux.

---

## [2026.3.0] — 2026-10-01

### 🦫 Castor Rebrand & Universal Model Agnosticism
- **Model-Agnostic Engine Core**: Decoupled the microkernel and runner from hardcoded assumptions. Supports any OpenAI-compatible provider (Ollama, LM Studio, vLLM, SGLang, LiteLLM) via `~/.castor/config.json`.
- **User Default Baseline Preserved**: Out-of-the-box zero-config defaults remain tuned for the flagship RTX 3090 / 4090 setup (`Qwen3.8-27B`, `18020`, 245K context, vLLM launcher).
- **Dual-Namespace Configuration**: `CASTOR_*` environment variables take precedence with automatic backwards-compatible fallback to `QWEN_*`.
- **Namespace-Protected MCP Tools**: Default stdio tool prefix updated to `castor` (`castor_coworker`, `castor_task`, `castor_server`) with dynamic schema generation.
- **Unified Package & CLI**: Renamed package directory to `mcp-castor/` and established `castor` CLI (`bin/castor.js`) with `install`, `init`, and `status` subcommands.
- **Multimodal Vision Integration**: Native vision support across both Lead Architect (Gemini 3.8 Flash) and local peer programmer at $0 token cost.
- **Authoritative Gate Verification**: 66 offline & integration test suites verified in a single pass.

---

## [2026.2.0] — 2026-09-26

**48 commits** between `v2026.1.0` and `v2026.2.0` (`e14a852` → `d34ea1a`).

### 🌟 Socratic Collaborative Pair-Programming & Cadence
- **Staff Engineer Peer Invariant** (`259be30`, `c5211fa`, `a5043b6`): Codified bidirectional Socratic collaboration between the cloud Lead Architect (Gemini 3.8 Flash / Claude Code) and local Qwen3.8-27B. Qwen operates as an autonomous Staff Software Engineer peer empowered to challenge flawed instructions, cite conflicting coordinates, evaluate trade-offs, and propose cleaner alternatives before mutating code.
- **Workspace Scratchpad Liberty** (`259be30`): Abolished "write no files" cognitive hoarding rules. Sanctioned `<workspace>/.scratch/` for intermediate data extractions, log slicing, and multi-file audit tables to prevent reasoning-context overflow.
- **Prompt Budget Enforcement at Gateway** (`7bf1a51`, `c5211fa`): Hardened MCP gateway to fail-fast reject monolithic dispatches exceeding 1,500 characters (`MonolithicDispatchRejected`), eliminating runaway multi-subsystem prompt dumping.
- **Dispatch Discipline Scorecard & Audit** (`d30f399`, `e09d8ed`, `e57ebd1`): Added `scripts/qwen_tasks_analysis.py` exporter to grade orchestration efficiency, tool ratios, and prefix cache affinity across production traces. Closed audit manifest (`75d0b80`, `b64d46d`).
- **Declarative Agent Contracts** (`128f96f`, `a0f62b0`): Refined agent-facing system guidelines to declarative "what is" specifications, removing superstitious line-ending chanting.

### 🔍 Native Multi-Provider Web & Framework Research
- **Multi-Provider Web Search (`web_search`)** (`1813cc8`, `0d1d271`): In-process multi-provider search engine with automatic failover and smart query classification:
  - **Brave Search**: Primary high-accuracy web search via API.
  - **Tavily**: Deep research extraction.
  - **Context7**: Version-accurate library and framework documentation via `isDocQuery` heuristic or `provider: "context7"`.
  - **SearXNG**: Self-hosted metasearch endpoint support.
  - **DuckDuckGo**: Zero-key fallback engine with leaky-bucket rate limiting to eliminate IP blocks.
- **Token-Dense Markdown Extraction (`web_fetch`)** (`1813cc8`): In-process article scraper utilizing Mozilla Readability and Turndown, stripping banner ads, navigation, and script noise into dense markdown.
- **Unified Global Configuration** (`0d1d271`): Cross-platform API key and endpoint management via `~/.qwen/config.json` with environment variable precedence.

### 🛡️ Cooperative Landing & Turn Ceiling Preservation
- **Cooperative Landing at Turn Ceilings** (`0d1d271`): Replaced abrupt process abortion (`turn_limit_reached`) on turn 100 with graceful Cooperative Landing. Tool execution is cleanly disabled, and the engine is prompted for mandatory deliverable synthesis.
- **Honest Status Taxonomy** (`0d1d271`, `f9a4a2d`): Concluded ceiling tasks return under status `completed_budget_exhausted` with a structured `[!WARNING]` advisory banner. Added `degenerate_response_truncated` for guard-truncated streams.
- **Proactive Turn 80 Rollover Advisory** (`e332cce`, `9b3ee41`, `0d1d271`): Injects an in-band `[!NOTE]` caution banner and `SessionTurnLimitRecommendation` at turn 80, prompting the orchestrator to checkpoint and roll to a fresh session ID.

### ⚡ KV Cache Prefix Stability & Architecture Hardening
- **Static System Prompt Anchoring** (`0d1d271`): Pruned ~2,800 characters of dynamic per-dispatch instructions from `castor_runner.js` to stabilize vLLM Automatic Prefix Caching (APC) hit rates at ~8,000–9,500 tok/s.
- **Deterministic Tool Serialization (M1)** (`32e54f4`): Normalized tool schemas and history reconstruction to maximize prefix reuse across multi-turn sessions.
- **Socket Persistence on `:18021`** (`0d1d271`): Resolved status server port churn on rapid restarts using `exclusive: true` (`SO_EXCLUSIVEADDRUSE`) and keeper/follower election.

### 🔒 In-Memory AST Gates & Action Space Rationalization (M2)
- **Invisible AST/LaTeX Syntax Gates** (`f42f1e9`): Pre-commit AST validation in `edit_file` for JavaScript, TypeScript, Python, JSON, LaTeX, and BibTeX, rejecting malformed syntax before disk write.
- **Lean 8-Tool Action Space** (`f42f1e9`): Pruned deprecated tool aliases (`exec_command`, `ast_replace`, `ast_replace_batch`), mounting Evo tools only when explicitly enabled.

### ⚙️ Engine Concurrency & Single-Tenant Execution
- **Strict Single-Tenant Default (`MAX_SEQS=1`)** (`4a5291a`): Reverted concurrency experimentation (`b352f8b`, `e9dc4fc`, `0d0fb9e`) back to strict single-tenant `MAX_SEQS=1` and `MAX_CONCURRENT_TASKS=1` to guarantee 100% VRAM and KV cache locality for active tasks.

### 🛑 Safety, Guards & Lifecycle Watchdogs
- **Binary File Read Guard** (`d65aeb4`): Pre-read magic-number detection throws `BinaryFileError` immediately, preventing binary byte pollution from corrupting LLM transcripts.
- **Explicit High-Reasoning Control** (`fd51654`, `ac212be`, `f956c00`): Made `reasoning_effort: "xhigh"` explicit-only; default remains `medium` to prevent token bloat during routine file editing.
- **Cross-Platform PID Liveness Probe** (`af68ce9`, `42a4228`, `2388236`): Heartbeat-guarded orphan detection with cross-platform PID checks (`wsl.exe` + Windows) and terminal `session_error` emission for dead processes.
- **Depth-Aware Watchdog Tiers** (`4e69bb3`, `4edadac`, `555328e`): Scaled stream-idle watchdogs up to 30 minutes for deep context, paired with exponential backoff on empty streams.
- **Clean Wait Contract** (`d481340`): Ensured `/wait` on `:18021` returns task failures as clean JSON 200 with socket termination.
- **7-Day Task Retention** (`c0ebd32`): Configurable task retention with background orphan sweeping.

### 📊 Economic Telemetry & Frontier Grounding
- **September 2026 Model Grounding** (`e75bed7`, `32fb458`, `6f01555`, `ff90452`): Recalibrated economic savings against September 2026 frontier flagships (Claude Opus 5.5, GPT-6 Astra, Claude Fable 5.1, Claude Sonnet 5, GLM 5.3) using exact Artificial Analysis rates.
- **Engine Usage Chunk Telemetry** (`263c7b3`, `ff90452`): Ingests exact `prompt_tokens` from vLLM usage chunks rather than heuristic character estimates.

---

## [2026.1.0] — 2026-09-09

**28 commits** between `v5.2.0` and `v2026.1.0` (`1e748bf` → `e14a852`).

### 🚀 Open-Source Launch & Standardizations
- **Castor Open-Source Documentation** (`5eea3e4`): Published comprehensive open-source documentation, architecture guides, and portable environment resolvers.
- **AGENTS.md AAIF Standard** (`5eea3e4`): Formalized `AGENTS.md` per the 2026 Agentic AI Foundation standard as the canonical, machine-readable contract across Claude Code, Google Antigravity, and Cursor.
- **GNU AGPLv3 Licensing** (`f802dff`, `7d66736`, `e14a852`, `139aaa8`): Adopted GNU AGPL-3.0-or-later dual-licensing to ensure open agent microkernel development.

### 🧪 Cross-Platform CI & Portability
- **Cross-Platform Matrix Gate** (`116ac88`, `68e7acb`): Established GitHub Actions CI matrix running across Node 22 & 24 on Ubuntu and Windows.
- **CI Test Portability** (`9fe4dd2`, `4193423`, `772b1ff`): Resolved path normalization, CWD fidelity, and CRLF quirks across platform runners.

### 🔒 Zero-Trust 5-Layer Sandboxing & Security Scrub
- **137-Vector Containment Suite** (`ae1d2fb`, `03d75e4`, `eea71fb`): Hardened 5-layer sandbox:
  1. Synchronous PathEscape Normalizer (`../../`, `C:\`, `/mnt/c`, device namespaces).
  2. Symlink Realpath Resolution (`fs.realpathSync`).
  3. Workspace Root Overwrite Guard.
  4. Dangerous Shell Filter (chaining, homoglyphs, fork bombs, `dd`).
  5. In-Memory AST Syntax Gates with instant rollback.
- **Credential & Path Scrub** (`ae1d2fb`, `03d75e4`): Completely eliminated hardcoded usernames, home directories, and tokens from repositories and tests.

### ⚡ Microkernel Hardening & Concurrency Locks
- **Diff & Patch Primitives** (`ad7ff2e`, `5f1ad31`): Adopted `apply_patch` for structural changes and auto-normalizing `edit_file` with LF preservation.
- **Multi-Instance Concurrency Locks** (`33f3d19`, `c3c8813`): Implemented atomic `O_EXCL` disk leases with Rename-to-Tombstone recovery, preventing race conditions across concurrent client sessions.
- **Honest Status Taxonomy** (`f3582be`, `b110602`): Dropped fragile string matching; introduced structured statuses (`completed_ceiling`, quarantined corrupted task logs).

### 📈 Production Marathon Telemetry & Baselines
- **13-Hour Production Marathon** (`3636436`, `eb042d5`, `9390851`, `c7a2b1a`): Verified 10.37M completion, 14.92M reasoning, and 962.3M prompt tokens delivered across 453 persistent sessions at $0 cost ($2,028+ saved vs Sonnet 5, $10,141+ saved vs frontier flagships).
- **Synchronized Rule Specifications** (`75ca32a`): Byte-identical rule locking across `CLAUDE.md` and `GEMINI.md` enforced by `tests/protocol_sync.test.js`.

---

## [5.2.0] — 2026-09-06

**1 commit** (`1e748bf`).

- **Castor Rebrand**: Officially renamed the microkernel to **Castor**.
- **Native Engine Consolidation**: Hard-wired native execution under `src/harness/`, pruned all legacy third-party runtime wrappers, and standardized in-process V8 tool dispatch.

---

## [5.1.1] — 2026-09-06

**7 commits** between `v5.1.0` and `v5.1.1` (`12c3683` → `d06e8fa`).

- **Single-Source Version Number** (`d06e8fa`): Version sourced dynamically from `package.json`.
- **Multi-Instance Process Isolation (P15)** (`493685f`, `79a581d`): Task cancellations sweep strictly anchored session IDs without killing other client instances' live child processes.
- **Loopback-Only Streaming Proxy** (`493685f`): Restricted port `:18022` strictly to loopback interface.
- **11-Hour Marathon Telemetry** (`8277aca`, `44f3279`, `1238e8a`): Documented 9,454 t/s prefill and 58.2 t/s generation baselines.

---

## [5.1.0] — 2026-09-06

**21 commits** completing Engineering Passes P1–P14 (`1975467` → `12c3683`).

- **P1–P4 (Platform & Shell Safety)** (`1975467`, `76b917c`, `06cc2f3`, `ee22c46`, `d6e6534`, `6c7a564`, `e8a58af`, `da6ee96`, `0ffa8f5`):
  - Single platform resolver (`src/platform.js`) and spawn-profile builder.
  - Windows DrvFs / WSL2 translation and realpath workspace canonicalization.
  - Protected-roots shell validator and 15 shell evasion fixes.
  - Deterministic line-ending enforcement via `.gitattributes` (`* text=auto eol=lf`).
  - `edit_file` uniqueness guards refusing ambiguous targets.
- **P5–P7 (AST Surgery & Stream Hardening)** (`ba1eb4b`, `dc03242`, `267befb`, `3004ca6`, `9c3b073`):
  - In-process structural AST engine via `@ast-grep/napi` with CLI fallback.
  - Universal syntax gates across JavaScript, TypeScript, Python, JSON, LaTeX, and BibTeX.
  - Batch AST replace with dry-run verification (`ast_replace_batch`).
  - Stream-death hardening, repetition breakers, and empty-response auto-recovery.
- **P8–P10 (Extensions & Process Reaping)** (`2ecf187`, `9a391db`, `96c6811`, `5efb490`):
  - Dynamic stdio `McpBridge` for external tools.
  - Packaged skills library with keyword auto-injection.
  - Anchored session-id sweep (`/proc/<pid>/cmdline`), kill certainty, double liveness escalation.
- **P11–P14 (Parity, Docs & Shell Hygiene)** (`eb5a215`, `10c1eab`, `8b3d103`, `12c3683`):
  - Offline syntax integrity scan (`node --check`), honest engine-down test skips, stdio purity lock.
  - MCP-conformant tool error envelopes (`isError: true`) and schema drift lock test (`tests/schema_parity.test.js`).
  - Architecture documentation rewrite and `docs/DESIGN.md` wisdom distillation.
  - Child spawn hygiene: stripping color-forcing env vars and enforcing `NO_COLOR=1` (P14).

---

## [4.5.9] — 2026-08-31

**Commit `f173fcf`**.

- **Dynamic Supervisor Tool Discovery**: Added `instructions.md` generation in `update_schemas.py` to guide orchestrators during lazy MCP tool schema resolution.

---

## [4.5.0 – 4.5.8] — 2026-08-29 to 2026-08-31

**15 commits** (`301bb68` → `ce44fe3`).

- **Universal Streaming Proxy (`:18022`)** (`da9159d`): Stateful UTF-8 stream proxy providing chunk resilience and SSE keep-alive.
- **Single Logical Concern Cadence** (`0e5c15f`, `dfbf43d`, `881cfb6`): Formalized Atomic Mutation Units (AMU), AST slice budgeting, and 3–4 file turn scoping.
- **Autonomous Warmup & Recovery** (`0a66263`, `e21cca0`, `70277e1`): HTTP-native warmup probe with drain recovery riding out large-prefill engine stalls.
- **Canary Health Gate & Gauges** (`301bb68`, `d4b915a`): Canary completion gate with `/metrics` Prometheus gauges.
- **Clean Subprocess Execution** (`6f92d01`): Switched invocation to `wsl.exe --exec` avoiding shell prompt corruption.
- **Multi-Turn Chat Primacy** (`dfb80be`): Established multi-turn conversation over warm prefix caches as the primary execution pattern.

---

## [4.0.0 – 4.4.1] — 2026-08-27 to 2026-08-29

**16 commits** (`78afe41` → `c356e3f`).

- **Zero-Turn Wait Architecture (`:18021`)** (`78afe41`): Hybrid execution yielding immediate sync returns (<15s) or durable task IDs with `/task/<id>/wait` long-poll endpoints.
- **Consolidated 3-Tool Architecture** (`2276965`): Unified sprawl into `qwen_coworker`, `qwen_task`, and `qwen_server`.
- **Disk-Lease Semaphore** (`a64559e`): Cross-process FIFO semaphore enforcing `MAX_CONCURRENT=1`.
- **Engine Wedge Auto-Heal** (`34d8b34`): Livelock detection with automatic process restarting and first-token timeouts.
- **Structured Task Status** (`c8d41ca`, `c577d2d`): Dynamic budget reporting, 10-minute timeout floor, and schema validation.
- **WSL Bash Routing** (`c356e3f`): Transparent Windows/WSL path translation and environment propagation.

---

## [3.4.0 – 3.5.0] — 2026-08-27

**2 commits** (`4ea52e3`, `8c1648f`).

- **Synchronous Execution & Vitality Checks** (`4ea52e3`, `8c1648f`): 1-hour heartbeat watchdog, pre-flight vitality probes, in-flight network retries, and cross-platform path normalization.

---

## [Initial Inception] — 2026-08-24 to 2026-08-26

**14 commits** (`30df2f1` → `0c67550`).

- **Stack Inception**: Initial local Qwen3.8-27B serving setup on RTX 3090 24GB (hybrid dense Gated-DeltaNet + attention, W4A16 AutoRound, DFlash2 speculative decoding, KVarN tiled KV cache).
- **Early Prototypes**: Prometheus telemetry collection, 21-hour marathon benchmarks, and LF line ending enforcement.
