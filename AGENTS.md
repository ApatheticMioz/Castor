# AGENTS.md — Castor

> **The vendor-neutral "README for machines."** This file is the canonical,
> machine-readable operating contract for any AI agent (Claude Code, Google
> Antigravity, Cursor, Codex, or a human) that reads, edits, tests, or
> optimizes this repository. It is the 2026 AAIF (Agentic AI Foundation)
> standard: a single source of truth that every agent persona must honor.
>
> **Precedence:** `AGENTS.md` > `CLAUDE.md` / `GEMINI.md` (persona-specific
> overlays) > `CONTRIBUTING.md` (human workflow) > `README.md` (marketing).
> If two files disagree, `AGENTS.md` wins and the discrepancy is a bug to file.

---

## 0. What This Repository Is

**Castor** is a local-first, **$0-token-cost** agent microkernel written in Rust that lets a
high-reasoning cloud orchestrator (the *Lead Architect*) drive a locally-served
peer programmer across any OpenAI-compatible serving engine (configured via pluggable
profiles: `llama.cpp`, `vLLM`, `SGLang`, `Ollama`, `LM Studio`, etc. — see [`docs/engine-profiles.md`](docs/engine-profiles.md))
to do code exploration, structural AST surgery, testing, and file editing — all inside a zero-trust sandbox.

```
Lead Architect (cloud: Claude / Gemini / Antigravity)
        |  MCP over stdio - 3 consolidated tools
        v
castor  (Rust MCP server + Castor microkernel)
        |  in-process tool calls (<0.1 ms) + zero-turn HTTP wait (:18021)
        v
Local Serving Engine (:18020)  ->  Pluggable Engine Profile (Bonsai / Qwen / Any)
```

The crate root lives at the repository root (`Cargo.toml`). A Node.js cross-platform shim
(`bin/castor.js`) is provided for npm and `npx` client integration.

---

## 1. Agent Personas

Three roles share this repo. Each has a distinct mandate; an agent must know
which one it is before acting.

| Persona | Runtime | Mandate | Hard Limits |
|---|---|---|---|
| **Lead Architect** | Cloud (Claude Code: GLM 5.3 text-only Plan / GLM 5.3-flash vision Exec; Antigravity: Gemini 3.8 Flash vision) | Architecture, task decomposition, plan & manifest authorship, supervisory steering, final synthesis. Proactively parallelizes independent work across concurrent coworker tasks. | **Never** bulk-read source into cloud context; **never** author code; offloads exploration in single-concern slices; sizes concurrent dispatches by workload tier (1 heavy / 2 medium / up to 4 short); **never** falls back to direct shell/file execution when coworker is offline. |
| **Autonomous Execution Coworker** | Local peer programmer (Ternary-Bonsai-2-27B or configured model) inside the sandboxed 16-tool Castor runtime | Hands-on execution & peer engineering: explore, structural AST surgery, file editing, git inspection/committing, stateful Python scratchpad, local visual inspection (images and PDFs), web/paper research, and shell — all inside the sandbox. Proactively challenges flawed assumptions, proposes architectural alternatives, and returns grounded facts. The 16 in-tree tools are: `read_file`, `write_file`, `edit_file`, `list_dir`, `search_code`, `ast_search`, `ast_replace`, `git_status`, `git_diff`, `git_log`, `git_commit`, `python_repl`, `paper_lookup`, `web_search`, `web_fetch`, `bash`. | **Never** authors high-level plans/roadmaps; **never** escapes the sandbox root. |
| **Autonomous Optimizer (Evo)** | Local, offline batch runner | Closed-loop mutation: propose -> evaluate -> select/revert against a deterministic fitness metric. | **Snapshot before mutating**; **revert on regression**; never edits files outside the candidate's snapshot list. |

> **Honest attribution:** No agent may claim "we" or coworker collaboration
> unless a `castor_coworker` MCP call was genuinely dispatched and its output
> incorporated.

---

## 2. Setup & Build Commands

All commands are **relative** — never hardcode a drive letter or home path.
`<repo>` = the root of your checkout.

### 2.1 Prerequisites
- **Rust >= 1.85** (Cargo, Rust 2024 edition).
- **Node.js >= 22** (optional, only for the `bin/castor.js` npm shim).
- **Git** (leave `core.autocrlf` at default — `.gitattributes` pins LF).
- **Optional, for the full live stack:** a GPU with >= 24 GB VRAM, Windows (or
  native Linux / WSL2), CUDA 12.4+, and an OpenAI-compatible serving engine (`llama.cpp` PRISM CUDA,
  vLLM, Ollama, LM Studio, SGLang, etc.; port `:18020` is the default).
  **You do NOT need a GPU to run the test gate** (see section 5).

### 2.2 Build
```bash
cargo build
```

### 2.3 Run the MCP server
```bash
# Directly via cargo
cargo run -- mcp

# Or via compiled binary
./target/debug/castor mcp

# Or via cross-platform Node shim
node bin/castor.js mcp
```
Registers three stdio tools: `castor_coworker`, `castor_task`, `castor_server`
(customizable via `CASTOR_TOOL_PREFIX`, default `"castor"`).
On first dispatch the serving engine boots (if configured) and the stream proxy
starts on `:18022`.

### 2.4 Register with a client
```bash
# Register automatically with Claude Code and Antigravity IDE
./target/debug/castor install --client all

# Or via Node shim
node bin/castor.js install --client all
```

### 2.5 Key environment variables & configuration

Configuration precedence (highest first): `CASTOR_*` environment
variables > `~/.castor/config.json` (or `$CASTOR_STATE_DIR/config.json`)
> built-in defaults. One key per setting — no aliases, no migration.
The exact list below is the present-state inventory from
`src/config.rs`; any variable not listed here does not exist.

| Variable | Default | Meaning |
|---|---|---|
| `CASTOR_MODEL` | `Ternary-Bonsai-2-27B-PQ2_0` | Target model identifier for the coworker (or configured profile). |
| `CASTOR_BASE_URL` | `http://127.0.0.1:18020/v1` | OpenAI-compatible endpoint. |
| `CASTOR_API_KEY` | `local-token` | API key sent to the engine (most local engines ignore it; required by strict endpoints). |
| `CASTOR_ENGINE_TYPE` | *(unset)* | Serving backend: `llama.cpp` \| `vllm` \| `openai`. |
| `CASTOR_PROFILE` | *(unset)* | Named profile selector: `bonsai-max` \| `hyperqwen-huge` (plus other builtin profiles) — overrides model/base URL/engine/slots/commands. |
| `CASTOR_MAX_CONTEXT` | `65536` | Maximum context window passed to the engine. |
| `CASTOR_LAUNCH_COMMAND` / `CASTOR_STOP_COMMAND` | *(profile-provided)* | Optional custom engine launch/stop command overrides (e.g. per-profile `.bat` / `vllm serve` invocations). |
| `CASTOR_BOOT_TIMEOUT_SECS` | `180` | Serving engine boot timeout in seconds before abort (default 3 minutes). |
| `CASTOR_PORT_ENGINE` | `18020` | Local serving engine port. |
| `CASTOR_PORT_STATUS` | `18021` | Zero-turn long-poll HTTP wait/status server. |
| `CASTOR_PORT_PROXY` | `18022` | Universal SSE streaming proxy (loopback only). |
| `CASTOR_TOOL_PREFIX` | `castor` | Namespace prefix for MCP tools (`castor_coworker`, etc.). |
| `CASTOR_STATE_DIR` | `~/.castor` | Root for task JSON, slot leases, session logs, Evo lineage. |
| `CASTOR_MAX_CONCURRENT_TASKS` | `1` | Cross-process execution slots (disk-lease semaphore). Slot defaults: 1 top-level, 4 for `bonsai-max`, 1 for `hyperqwen-huge`. |
| `CASTOR_MIN_EVICTION_DETECT_TOKENS` | `13056` | Minimum prompt tokens before KV-cache eviction detection engages. |
| `CASTOR_CACHE_HIT_YIELD_RATIO` | `0.20` | KV prefix-cache hit ratio that justifies yielding a warm slot to a new task. |
| `CASTOR_WORKSPACE_SKELETON` | `true` | Prepend a compact workspace-skeleton block to the system prompt (warm Turn-1 prefix for KV prefix caching). |
| `CASTOR_TOOL_SPILL_BYTES` | `16384` | Max bytes of a single tool output before it is spilled to disk (head/tail preview kept in conversation). |
| `CASTOR_DGI_REJECT_MARGIN` | `1.5` | DGI gatekeeper log-odds reject margin: reject only when `logP(OVERLOADED) − logP(ADMIT)` exceeds this. |
| `CASTOR_DGI_BYPASS_WARM_SESSION` | `true` | Skip the DGI probe for continuations that already have a non-empty session. |
| `CASTOR_DGI_TIMEOUT_SECS` | `30` | Timeout for the DGI 1-forward-pass logit probe. |
| `CASTOR_PROBE_BUDGET` | `6` | Consecutive non-mutating `bash` probes before a probe-budget advisory is injected. |
| `CASTOR_TURN_TIMEOUT_SECS` | `600` | Per-turn wall-clock watchdog timeout; `0` disables. |
| `CASTOR_MAX_TURN_REASONING_TOKENS` | `16384` | Per-turn reasoning token budget before a "stall" is declared; `0` disables. |
| `CASTOR_REASONING_OPEN_TAG` | `<th` + `ink>` | Reasoning block open tag delimiter (source-assembled to avoid the reserved token sequence in this codebase). |
| `CASTOR_REASONING_CLOSE_TAG` | `</th` + `ink>` | Reasoning block close tag delimiter. |
| `CASTOR_SEARXNG_URL` | `http://127.0.0.1:8888/` | Local SearXNG meta-search instance endpoint (first `web_search` provider). |
| `CASTOR_BRAVE_API_KEY` | *(unset)* | Optional Brave Search API key — the `web_search` fallback when SearXNG is not configured. |
| `CASTOR_OPENALEX_EMAIL` | *(unset)* | Polite-pool contact email for OpenAlex `paper_lookup`. |
| `CASTOR_OPENALEX_API_KEY` | *(unset)* | Optional OpenAlex API key for higher rate limits. |

---

## 3. Code Style & AST Patterns

### 3.1 Language & formatting
- **Rust**: 4-space indent, 2024 edition, strictly enforced by `cargo fmt` and `cargo clippy --all-targets -- -D warnings`.
- **Line endings are a hard invariant** (Rule 7): `* text=auto eol=lf` in
  `.gitattributes`. `*.bat` / `*.cmd` are the only CRLF files.

### 3.2 Structural AST surgery (preferred over regex)
Use structural AST tools, not string matching, for syntactic discovery and refactoring:
- `ast_search` — find code by pattern with metavariables via native `ast-grep` (`function $NAME($$$ARGS) { $$$BODY }`).
- `ast_replace` — multi-file structural AST refactoring with automatic rollback on syntax error.
- Code edits made via `edit_file` are automatically validated by in-memory AST
  and syntax gates before disk commit.

### 3.3 Edit primitives
- `edit_file` — exact-substring, uniqueness-guarded, per-region line-ending
  preservation with transparent in-memory syntax validation.
- Fuzzy matching is not supported; modifications use exact substrings.

### 3.4 Module layout

Feature flags (`Cargo.toml`): `default = ["mcp"]`; `mcp`, `evals`,
`evo` (= `evals`), `full` (= `mcp` + `evo` + `evals`).

```
Cargo.toml               # Single root crate manifest (dual-target: lib + bin)
bin/
  castor.js              # Node.js cross-platform distribution shim
src/
  lib.rs                 # Library crate root (public modules, traits, and programmatic embedding)
  main.rs                # Thin CLI binary entry point (delegates to castor::cli::run)
  cli.rs                 # Clap CLI parsing, command routing, and internal worker dispatch
  config.rs              # Configuration loader (serde, env > json > defaults)
  platform.rs            # Cross-OS path translation and process tree management
  skills.rs              # agentskills.io SKILL.md indexer
  telemetry.rs           # JSONL event ledger & derived statistics
  pruner.rs              # State dir retention pruner
  state/
    mod.rs               # State dir layout, locks, and session persistence
  mcp/
    mod.rs               # rmcp stdio transport, tool schemas, and server
    dgi.rs               # Decomposition Granularity Index (DGI) 1-forward pass logit probe
    worker.rs            # Detached worker process execution loop
  task/
    mod.rs               # Task management subsystem
    registry.rs          # Task lifecycle, disk mirror synchronization, and persistence
    wait.rs              # Long-poll HTTP wait endpoint (:18021)
    semaphore.rs         # File-backed cross-process slot semaphore
  proxy/
    mod.rs               # Stream proxy server (:18022)
    sanitize.rs          # SSE UTF-8 reassembly, repetition detector, error frame translation
  engine/
    mod.rs               # Provider interface and HTTP client
    backend.rs           # Serving backend capability matrix (llama.cpp / vLLM / OpenAI)
    lifecycle.rs         # Engine boot, canary verification, and auto-heal
    provider.rs          # OpenAI / vLLM streaming chat client
    supervisor.rs        # Engine supervisor (start/stop/status, auto-heal supervision)
    detached.rs          # Detached engine process supervision
  runner/
    mod.rs               # Multi-turn autonomous agent loop
    events.rs            # Session event ledger writer and replayer
    loop_detector.rs     # Action-hash sliding window loop breaker
  tools/
    mod.rs               # Composite tool executor (16 builtin tools) implementing runner::ToolExecutor
    fs.rs                # Filesystem tools (read, write, edit, list, search)
    sandbox.rs           # 5-layer path containment and 137-vector security policy
    shell.rs             # Safe bash executor and command AST validator
    ast.rs               # Structural AST search and replace via native ast-grep
    git.rs               # Native git status/diff/log/commit primitives
    repl.rs              # Stateful Python REPL scratchpad
    web.rs               # Web search (SearXNG -> Brave) and fetch (markdown conversion)
    paper.rs             # Academic paper lookup via OpenAlex (metadata, affiliations, OA PDFs)
    extensions.rs        # MCP extension bridges (rmcp child processes)
  evals/
    mod.rs               # Eval harness root
    fixture.rs           # Deterministic fixture engine for offline replay
    replay.rs            # Session replay for Tier A offline evals
    runner.rs            # Batch eval runner against task suites
  evo/
    mod.rs               # Offline evolutionary optimizer subsystem
    lineage.rs           # DAG of scored commits and parent pointers
    optimizer.rs         # Reflective batch evaluation against task suites
    watchdog.rs          # Lineage stagnation and health detector
evals/                   # Tier A offline replay, golden test tasks, and DGI 187-prompt benchmark
skills/                  # Reusable SKILL.md workflow recipes
```

### 3.5 Documentation & Rustdoc Standards (Present-State Truth)
- **Zero Storytelling / No Historical Changelogs / No Defensive Lore**: Comments and docstrings must document the current software architecture, behavior, and invariants strictly as-is in present-state truth.
- **Strictly Forbidden Commentary Patterns**:
  - Chronological narratives: "Raised from X to Y", "Reverted from A to B", "Historically", "Formerly", "The old code used to...".
  - Calendar dates or version snapshots: "2026-09-12:", "v1.2:".
  - Milestone or ticket tags: "M6a:", "P15:", "FX2:", "Issue #51:", "PR #13:".
  - Lab notes or exploratory diaries: "Measured on our RTX 3090...", "A/B tested with 6 trials...".

---

## 4. Invariant Boundaries

### 4.1 ALWAYS
- **ALWAYS** run the test gate before committing (section 5).
- **ALWAYS** keep every path relative or env-driven. Never hardcode a drive letter, home dir, or username.
- **ALWAYS** surface the unadulterated error (Rule 8: fail-fast, zero-masking).
- **ALWAYS** keep `CLAUDE.md` and `GEMINI.md` shared invariants byte-identical.
- **ALWAYS** use the sanctioned workspace scratchpad (`<workspace>/.scratch/` or repository-local helper scripts) for empirical reproduction scripts, intermediate data extractions, log slicing, and multi-item audit ledgers.
- **ALWAYS** keep `Cargo.toml` and `package.json` version strings byte-identical before creating or pushing a release tag.
- **ALWAYS** actively monitor CI/CD workflow runs to terminal completion (`gh run watch` or Actions console) when dispatching a release tag; never fire-and-forget.

### 4.2 ASK FIRST
- **ASK** before adding a new dependency.
- **ASK** before changing any constant in `src/config.rs` (ports, timeouts, budgets).
- **ASK** before touching the 5-layer sandbox, the semaphore, or the wedge detector.
- **ASK** before committing anything under `.evo/`, `.scratch/`, or `*.log`.

### 4.3 NEVER
- **NEVER** let a file or shell operation escape the workspace root. The 5-layer defense is non-negotiable.
- **NEVER** dump raw binary file bytes (`.png`, `.jpg`, `.exe`, etc.) into plaintext string buffers or transcripts (`BinaryFileError` guard). PDF documents are natively parsed in-memory into clean UTF-8 text by both `read_file` (local filesystem) and `web_fetch` (online URLs) via `lopdf`.
- **NEVER** silently catch, suppress, or mask errors or upstream HTTP status codes (Rule 8).
- **NEVER** weaken, skip, or delete a failing test to "unblock" the build.
- **NEVER** interrupt, probe, or reboot a running serving engine during the default test gate. The default suites run fully offline; the live GPU tests (`tests/llama_cpp.rs`, `tests/bonsai_deep_eval.rs`) are `#[ignore]`-gated and additionally require the explicit `ALLOW_ENGINE_INTERRUPT=1` opt-in to execute.
- **NEVER** substitute cloud shell or file tools for coworker execution when the coworker is offline or fails; halt and notify the user or boot via 'castor server start'.

---

## 5. Verification Procedures

### 5.1 The test gates
```bash
# Offline test gate (unit + integration, deterministic mock/replay harnesses)
cargo test --all-features

# Strict clippy gate (all targets, all features, zero warnings)
cargo clippy --all-targets --all-features -- -D warnings
```

**Zero Engine Interruption Invariant:**
The default `cargo test --all-features` gate runs **offline** against
deterministic mock/replay harnesses and never interrupts, probes, or
reboots a running serving engine. The live GPU tests in
`tests/llama_cpp.rs` and `tests/bonsai_deep_eval.rs` are `#[ignore]`-gated
(they do not run in the default suite) and additionally require the
explicit `ALLOW_ENGINE_INTERRUPT=1` opt-in before they execute.

---

## 6. Security & Reporting

- The sandbox is a **zero-trust, 5-layer** defense-in-depth boundary:
  PathEscape validation -> Symlink Realpath resolution -> Root-Overwrite Guard -> Dangerous-Shell Filter -> AST Syntax Validation & Evo Rollback.
- **Never** open a public issue for a security vulnerability. Report
  privately per [`SECURITY.md`](SECURITY.md).
- The 137-vector containment suite (`src/tools/sandbox.rs`: 123 attack
  vectors blocked, 14 allow vectors) is the regression net for the boundary.

---

## 7. Ground-Truth Hierarchy

1. **Active source code**, `Cargo.toml`, and build artifacts = ground truth.
2. `AGENTS.md`, `CLAUDE.md`, `GEMINI.md`, `CONTRIBUTING.md` = operating
   contracts (verify against code before relying on them).
3. Secondary documentation, historical audit reports, and markdown notes =
   **reference ledgers, NOT executable ground truth.**

---

## 8. Release & Packaging Contract (npm OIDC & Cargo)

Castor releases follow strict automated OIDC provenance and package registry invariants:

### 8.1 The Triple-Lock Version Invariant
The project version is represented in three distinct locations that **must remain byte-identical** on every release:
1. `Cargo.toml`: `version = "X.Y.Z"` (compiled into the binary, reported by `castor --version`).
2. `package.json`: `"version": "X.Y.Z"` (read directly by npm CLI during packaging and tarball generation).
3. Git Tag: `vX.Y.Z` (triggers `.github/workflows/release.yml`, parsed into GitHub Release title).

### 8.2 npm OIDC Trusted Publishing Architecture (`--provenance`)
- **Zero Static Tokens**: Releases do NOT use a static `NPM_TOKEN` secret. Instead, npm authenticates via GitHub Actions OpenID Connect (OIDC) (`permissions: id-token: write`).
- **Cryptographic Attestation**: When `npm publish --provenance` runs, npm requests a signed JWT from GitHub's OIDC provider containing the repository (`ApatheticMioz/Castor`), the git ref (`refs/tags/vX.Y.Z`), and the exact commit SHA.
- **Sigstore Transparency**: npm mints a short-lived certificate via Sigstore Fulcio and records the provenance attestation in the Rekor public ledger, verifying that the published package originated from this exact workflow run.
- **Registry Immutability & Fail-Fast**: The npm registry enforces strict version immutability. Once a version is published, it can never be replaced. If `package.json` has not been bumped to match the git tag, `npm publish` fails fast with `403 Forbidden` (`You cannot publish over previously published versions`). Subsequent promotion (`npm dist-tag add`) will fail with `404 Not Found`.

### 8.3 Mandatory CI/CD Supervisory Monitoring
- **No Fire-and-Forget Tagging**: Pushing a version tag dispatches asynchronous CI/CD. The operator or autonomous agent driving the release is **strictly required to monitor the pipeline to terminal completion**:
  ```bash
  # Watch the active release run until conclusion
  gh run watch
  ```
- **Terminal Status Inspection**: If the release fails (test gate failure, clippy lint, Node shim invocation failure, OIDC token minting timeout, or npm dist-tag promotion failure), the supervisor must inspect `gh run view --log-failed` immediately and diagnose root cause before attempting remediation.

### 8.4 Atomic Release Execution Checklist
1. **Pre-flight verification**: Run `cargo test --all-features` (0 failures) and `cargo clippy --all-targets --all-features -- -D warnings` (0 warnings).
2. **Lockstep version bump**: Update `Cargo.toml` (`version = "X.Y.Z"`) and `package.json` (`"version": "X.Y.Z"`).
3. **Commit & push main**:
   ```bash
   git commit -am "chore(release): vX.Y.Z"
   git push origin main
   ```
4. **Tag & push tag**:
   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
5. **Supervise**: Run `gh run watch` and verify that the release job finishes with exit code 0 and promotes `latest` on npm.

