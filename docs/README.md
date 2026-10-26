# Documentation Index

This directory holds the **active specifications** for the Castor harness. All documents describe present-state architecture and are maintained in lockstep with the code.

---

## Active Specifications

- [`engine-profiles.md`](engine-profiles.md) — Pluggable serving-engine profiles (`bonsai-max`, `hyperqwen-huge`, generic OpenAI endpoints) and the `CASTOR_ENGINE_TYPE` / `CASTOR_PROFILE` configuration contract.
- [`llama-cpp.md`](llama-cpp.md) — Managed `llama.cpp` (`llama-server`) startup/shutdown, GGUF models, tool-calling flags, and optional live `#[ignore]`-gated tests.

---

## Related References

- **MCP Server Architecture**: [`src/mcp/mod.rs`](../src/mcp/mod.rs) & [`src/main.rs`](../src/main.rs)
- **Canonical Operating Contract**: [`AGENTS.md`](../AGENTS.md)
- **Benchmark Suites**: [`benchmarks/`](../benchmarks/) (SWE-bench validation)

