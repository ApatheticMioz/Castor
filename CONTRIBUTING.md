# Contributing to Castor

Thank you for your interest in contributing to **Castor** — the Universal 245K
Agent Microkernel for Local LLMs. This guide gives you a frictionless path from
clone to merged pull request.

> **Machine-readable contract:** AI agents (Claude Code, Antigravity, Cursor,
> Codex) should read [`AGENTS.md`](AGENTS.md) first — it is the canonical
> operating contract. This file is the human workflow.

---

## 1. Local Environment Setup

### Prerequisites
- **Rust >= 1.85** (Cargo, Rust 2024 edition).
- **Git** (leave `core.autocrlf` at default; `.gitattributes` pins LF).
- **Node.js >= 22** (optional, only for the `bin/castor.js` npm distribution shim).
- **Optional, for the full live stack only:** a GPU with >= 24 GB VRAM, WSL2
  (or native Linux), CUDA 12.4+, and an OpenAI-compatible serving engine (vLLM, Ollama,
  LM Studio, SGLang, etc.; Qwen3.8-27B on `:18020` by default).
  **You do NOT need a GPU to contribute** — the entire test gate runs offline.

### Clone & Build
```bash
git clone https://github.com/ApatheticMioz/Castor.git
cd Castor

# Build debug binary
cargo build

# (Optional) Build release binary
cargo build --release
```

### MCP Server Registration
Register the compiled binary with your preferred AI client:
```bash
# Register automatically with Claude Code and Antigravity IDE
./target/debug/castor install --client all

# Or via cross-platform Node shim
node bin/castor.js install --client all
```

---

## 2. Running Tests (No GPU Required)

The test gate is the single source of truth. Run it **before** opening a PR.

```bash
# Fast offline test gate - 243 unit and integration tests (~7s).
cargo test

# Strict linter gate - zero warnings allowed
cargo clippy --all-targets -- -D warnings

# Format check
cargo fmt -- --check
```

**Zero Engine Interruption Invariant:**
By default, tests NEVER interrupt, probe, or reboot a running serving engine (`ALLOW_ENGINE_INTERRUPT=0`). Live GPU execution is gated behind the explicit dangerous override `ALLOW_ENGINE_INTERRUPT=1`.

### Targeted Test Runs
Run a specific module or test in isolation:
```bash
# 137-vector zero-trust containment suite
cargo test tools::sandbox::tests::security_vector_table_137

# AST search and replace with rollback
cargo test tools::ast::tests

# Slot semaphore cross-process exclusion
cargo test task::semaphore::tests

# Stream proxy & SSE UTF-8 sanitizer
cargo test proxy::sanitize::tests

# Long-poll HTTP wait endpoint (:18021)
cargo test task::wait::tests
```

### Cross-platform
The gate runs identically on **Windows 11** and **WSL2 / Linux**. When touching path handling, verify on both platforms.

---

## 3. Architecture Overview

Castor is a high-performance **agent microkernel** in Rust exposed over standard Model Context Protocol (MCP) stdio:

```
Lead Architect (cloud: Claude / Gemini / Antigravity)
        |  MCP over stdio - 3 consolidated tools
        v
castor  (Rust MCP server + Castor microkernel)
        |  in-process tool calls (<0.1 ms) + zero-turn HTTP wait (:18021)
        v
Local Serving Engine (WSL2 / Linux, :18020)  ->  Serving Provider (Qwen3.8-27B default)
```

### Module Layout
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
    extensions.rs        # MCP extension bridges (rmcp child processes)
  evo/
    mod.rs               # Offline evolutionary optimizer subsystem
    lineage.rs           # DAG of scored commits and parent pointers
    optimizer.rs         # Reflective batch evaluation against task suites
    watchdog.rs          # Lineage stagnation and health detector
evals/                   # Tier A offline replay and golden test tasks
skills/                  # Reusable SKILL.md workflow recipes
```

---

## 4. Commit & Pull-Request Lifecycle

### Conventional Commits
We use [Conventional Commits](https://www.conventionalcommits.org/):
- `feat:` a new feature or capability.
- `fix:` a bug fix.
- `refactor:` restructuring without behavioral change.
- `test:` adding or updating test suites.
- `docs:` documentation updates and audit logs.
- `perf:` performance optimizations (latency, VRAM, throughput).
- `ci:` / `chore:` / `build:` for infrastructure, housekeeping, and packaging.

### PR Checklist
Before requesting review, confirm:
- [ ] `cargo test` is 100% green (277/277 tests).
- [ ] `cargo clippy --all-targets -- -D warnings` passes with zero warnings.
- [ ] `cargo fmt -- --check` passes cleanly.
- [ ] No hardcoded drive letters, home dirs, or usernames in any file (paths are relative or env-driven).
- [ ] No secrets, credentials, or machine-identity paths committed.
- [ ] Documentation and rustdocs adhere to **present-state truth** (zero storytelling, no historical changelogs, no milestone tags in comments).
- [ ] Line endings are LF (`.gitattributes` enforces; `*.bat`/`*.cmd` are the only CRLF files).
- [ ] Commit messages follow Conventional Commits.

---

## 5. Reporting Vulnerabilities

**Do not open a public issue for a security vulnerability.** Report it
privately per [`SECURITY.md`](SECURITY.md), which lists the supported
versions, the private reporting channel, and our response SLA. The 137-vector
containment suite (`src/tools/sandbox.rs`) is the regression net for the
zero-trust boundary.

---

## 6. Release & Versioning Lifecycle (npm OIDC & Cargo)

Castor uses [Semantic Versioning 2.0.0](https://semver.org/) (`MAJOR.MINOR.PATCH`):
- **MAJOR**: Breaking changes to MCP tool signatures, protocol breaking removals.
- **MINOR**: Backward-compatible new features (e.g. new CLI subcommands, new tool parameters, auto-boot).
- **PATCH**: Backward-compatible bug fixes and internal stability hardening.

### Automated Release Pipeline (`.github/workflows/release.yml`)
Releases are automatically triggered whenever a git tag matching `v*` is pushed to GitHub:
1. Runs full test suite (`cargo test --verbose`), clippy gate (`-D warnings`), and Node.js cross-platform shim test (`node bin/castor.js --help`).
2. Creates the GitHub Release draft with auto-generated release notes.
3. Publishes `mcp-castor` to npm using **OIDC Trusted Publishing** (`--provenance`) with cryptographic Sigstore attestation.

### ⚠️ Version Invariant & Fail-Fast Guard
- npm package releases are **immutable**. Once published, a version can never be republished or overwritten.
- `npm publish` parses the version strictly from `package.json`. If `package.json` does not match the git tag, the publish step fails with `403 Forbidden`.
- **`Cargo.toml` (`version`), `package.json` (`version`), and the git tag (`vX.Y.Z`) MUST remain byte-synchronized.**

### Mandatory CI/CD Monitoring
Pushing a release tag is an active operation, not fire-and-forget. The contributor or agent cutting a release must monitor GitHub Actions until completion:
```bash
# Watch the release workflow run to terminal status
gh run watch
```
