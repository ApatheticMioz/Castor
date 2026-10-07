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
**Qwen3.8-27B** peer programmer to do code exploration, structural AST
surgery, testing, and file editing — all inside a zero-trust sandbox.

```
Lead Architect (cloud: Claude / Gemini / Antigravity)
        |  MCP over stdio - 3 consolidated tools
        v
castor  (Rust MCP server + Castor microkernel)
        |  in-process tool calls (<0.1 ms) + zero-turn HTTP wait (:18021)
        v
Local Serving Engine (WSL2 / Linux, :18020)  ->  Serving Provider (Qwen3.8-27B default)
```

The crate root lives at the repository root (`Cargo.toml`). A Node.js cross-platform shim
(`bin/castor.js`) is provided for npm and `npx` client integration.

---

## 1. Agent Personas

Three roles share this repo. Each has a distinct mandate; an agent must know
which one it is before acting.

| Persona | Runtime | Mandate | Hard Limits |
|---|---|---|---|
| **Lead Architect** | Cloud (Claude Code: GLM 5.3 text-only Plan / GLM 5.3-flash vision Exec; Antigravity: Gemini 3.8 Flash vision) | Architecture, task decomposition, plan & manifest authorship, supervisory steering, final synthesis. | **Never** bulk-read source into cloud context; **never** author code; offloads exploration to the Coworker in single-concern slices. |
| **Autonomous Execution Coworker** | Local peer programmer (Qwen3.8-27B default, or configured model) via Castor (`castor` MCP) | Hands-on execution & peer engineering: explore, AST surgery, edit, test, shell. Local peer programmer also has vision capability enabled. Proactively challenges flawed assumptions, proposes architectural alternatives, and returns grounded facts. | **Never** authors high-level plans/roadmaps; **never** escapes the sandbox root. |
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
- **Optional, for the full live stack:** a GPU with >= 24 GB VRAM, WSL2 (or
  native Linux), CUDA 12.4+, and an OpenAI-compatible serving engine (vLLM, Ollama,
  LM Studio, SGLang, etc.; Qwen3.8-27B on `:18020` is the default).
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
| Variable | Default | Meaning |
|---|---|---|
| `CASTOR_MODEL` | `Qwen3.8-27B` | Target model identifier for the coworker. |
| `CASTOR_BASE_URL` | `http://127.0.0.1:18020/v1` | OpenAI-compatible endpoint. |
| `CASTOR_TOOL_PREFIX` | `castor` | Namespace prefix for MCP tools (`castor_coworker`, etc.). |
| `CASTOR_MAX_CONCURRENT` | `1` | Cross-process execution slots (disk-lease semaphore). |
| `CASTOR_STATE_DIR` | `~/.castor` | Root for task JSON, slot leases, session logs, Evo lineage. |
| `STATUS_PORT` | `18021` | Zero-turn long-poll HTTP wait/status server. |
| `STREAM_PROXY_PORT` | `18022` | Universal SSE streaming proxy (loopback only). |
| `CASTOR_REASONING_EFFORT` | `medium` | Fallback effort when a dispatch sends none {xhigh, medium, low}. |
| `TEST_OFFLINE` | *(unset)* | Set to `1` to force live suites to skip (GPU-less runs). |
| `ALLOW_ENGINE_INTERRUPT` | `0` | Dangerous override: by default, test suites NEVER interrupt, probe, or reboot vLLM. |
| `CASTOR_BASE_TURN_BUDGET` | `80` | Base turn budget before requiring supervisor lease extension or landing. |
| `CASTOR_MAX_ELASTIC_TURNS` | `200` | Maximum allowed turn ceiling via supervisor lease extension. |
| `CASTOR_BOOT_TIMEOUT_SECS` | `180` | Serving engine boot timeout in seconds before abort (default 3 minutes). |
| `CASTOR_SEARXNG_URL` | `http://127.0.0.1:8888/` | Local SearXNG meta-search instance endpoint. |
| `CASTOR_BRAVE_API_KEY` | *(unset)* | Optional Brave Search API subscription key for fallback. |
| `CASTOR_OPENALEX_EMAIL` | *(unset)* | Polite-pool contact email for zero-hallucination paper lookups. |
| `CASTOR_OPENALEX_API_KEY` | *(unset)* | Optional OpenAlex Premium API key for high-throughput institutional tiers. |

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
```
Cargo.toml               # Single root crate manifest
bin/
  castor.js              # Node.js cross-platform distribution shim
src/
  main.rs                # CLI entry point (clap dispatch, worker subcommand)
  config.rs              # Configuration loader (serde, env > json > defaults)
  platform.rs            # Cross-OS path translation and process tree management
  skills.rs              # agentskills.io SKILL.md indexer
  telemetry.rs           # JSONL event ledger & derived statistics
  pruner.rs              # State dir retention pruner
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
    lifecycle.rs         # Engine boot, canary verification, and auto-heal
    provider.rs          # OpenAI / vLLM streaming chat client
  runner/
    mod.rs               # Multi-turn autonomous agent loop
    events.rs            # Session event ledger writer and replayer
    loop_detector.rs     # Action-hash sliding window loop breaker
  tools/
    mod.rs               # Composite tool executor implementing runner::ToolExecutor
    fs.rs                # Filesystem tools (read, write, edit, list, search)
    sandbox.rs           # 5-layer path containment and 137-vector security policy
    shell.rs             # Safe bash executor and command AST validator
    ast.rs               # Structural AST search and replace via native ast-grep
    web.rs               # Web search (SearXNG -> Brave -> DDG) and fetch (markdown conversion)
    paper.rs             # Academic paper lookup via OpenAlex (metadata, affiliations, OA PDFs)
    extensions.rs        # MCP extension bridges (rmcp child processes)
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
- **NEVER** interrupt the running vLLM engine, fire completion probes into `:18020`, or issue stop/reboot commands during testing unless `ALLOW_ENGINE_INTERRUPT=1` is explicitly set.

---

## 5. Verification Procedures

### 5.1 The test gates
```bash
# Fast test gate (~7s, offline, zero engine interruption)
cargo test

# Full clippy check (strict zero-warning gate)
cargo clippy --all-targets -- -D warnings
```

**Zero Engine Interruption Invariant:**
By default, tests NEVER interrupt, probe, or reboot a running serving engine (`ALLOW_ENGINE_INTERRUPT=0`). Live GPU execution is gated behind the explicit dangerous override `ALLOW_ENGINE_INTERRUPT=1`.

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
1. **Pre-flight verification**: Run `cargo test` (277+ tests green) and `cargo clippy --all-targets -- -D warnings` (0 warnings).
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

