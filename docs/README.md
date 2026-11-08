# Documentation Index

This directory holds the **active specifications** for the Castor harness. All documents describe present-state architecture and are maintained in lockstep with the code.

---

## Guides

- [`getting-started.md`](getting-started.md) — Install, MCP client registration, connecting a local serving engine, and the supported client/engine matrix.
- [`architecture.md`](architecture.md) — System flow, DGI, zero-turn lifecycle, token-budget semaphore, stream proxy, Evo, the 8-tool core, skills, and the MCP tool surface.
- [`security.md`](security.md) — The zero-trust workspace containment pipeline.
- [`cli.md`](cli.md) — CLI reference, building, and the test gate.

## Active Specifications

- [`engine-profiles.md`](engine-profiles.md) — Pluggable serving-engine profiles (`bonsai-max`, `hyperqwen-huge`, generic OpenAI endpoints) and the `CASTOR_ENGINE_TYPE` / `CASTOR_PROFILE` configuration contract.
- [`llama-cpp.md`](llama-cpp.md) — Managed `llama.cpp` (`llama-server`) startup/shutdown, GGUF models, tool-calling flags, and optional live `#[ignore]`-gated tests.

---

## Related References

- **MCP Server Architecture**: [`src/mcp/mod.rs`](../src/mcp/mod.rs) & [`src/main.rs`](../src/main.rs)
- **Canonical Operating Contract**: [`AGENTS.md`](../AGENTS.md)
- **Benchmark Suites**: [`benchmarks/`](../benchmarks/) (SWE-bench validation)
