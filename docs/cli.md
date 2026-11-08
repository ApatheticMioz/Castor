# CLI Reference & Development

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

Additional subcommands include `castor status` (zero-turn HTTP wait server on `:18021`) and `castor proxy` (SSE stream proxy on `:18022`). See [`src/cli.rs`](../src/cli.rs) for full CLI definitions.

`castor stats` is the source of the savings figures shown in the project README. Run it
against your own ledger to measure your own savings.

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

The full environment-variable inventory and contributor contract live in
[`AGENTS.md`](../AGENTS.md) and [`CONTRIBUTING.md`](../CONTRIBUTING.md).
