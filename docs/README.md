# Documentation Index

This directory holds the **active specifications** for the Castor harness
plus the **historical reference ledgers** (audits and archive notes). The two
kinds of documents are intentionally separated by directory.

> [!NOTE]
> `docs/audits/` and `docs/archive/` are **historical reference ledgers** —
> time-stamped point-in-time analyses, forensic post-mortems, and
> implementation ledgers (including pre-Rust `mcp-qwen` / `mcp-castor`
> JS-runtime analyses). They are **NOT active specification**. For the
> current architecture, configuration, and behavior, consult the active
> specs below plus the source of truth in `src/` and `AGENTS.md`
> (see the Ground-Truth Hierarchy in `AGENTS.md` §7).

---

## Active Specifications

These documents describe the present-state system and are maintained in
lockstep with the code:

- [`engine-profiles.md`](engine-profiles.md) — Pluggable serving-engine
  profiles (`bonsai-max`, `hyperqwen-huge`, generic OpenAI endpoints) and
  the `CASTOR_ENGINE_TYPE` / `CASTOR_PROFILE` configuration contract.
- [`llama-cpp.md`](llama-cpp.md) — Managed `llama.cpp` (`llama-server`)
  startup/shutdown, GGUF models, tool-calling flags, and optional live
  `#[ignore]`-gated tests.

---

## Historical Audit Ledger (`docs/audits/`)

Time-stamped forensic, adversarial, and progress audits. Ordered roughly by
date. These record what was done and why — they are a reference, not an
executable contract.

| Report | Focus |
|---|---|
| [`AUDIT_2026-09-05.md`](audits/AUDIT_2026-09-05.md) | Full repository & configuration drift audit. |
| [`PATCHWORK_ADVERSARIAL_AUDIT_2026-09-05.md`](audits/PATCHWORK_ADVERSARIAL_AUDIT_2026-09-05.md) | Adversarial threat modeling of the AST / shell / file-mutation boundary (pre-Rust JS runtime). |
| [`AUDIT_MANIFEST_2026-09-08.md`](audits/AUDIT_MANIFEST_2026-09-08.md) | Findings manifest for the 2026-09-08 session (divergent protocol files, line-ending defects). |
| [`REBRAND_AND_ISSUE_AUDIT_2026-09-08.md`](audits/REBRAND_AND_ISSUE_AUDIT_2026-09-08.md) | Rename/rebrand audit and open-issue triage. |
| [`CROSS_SESSION_VLLM_AND_QWEN_DISPATCH_AUDIT_2026-09-13.md`](audits/CROSS_SESSION_VLLM_AND_QWEN_DISPATCH_AUDIT_2026-09-13.md) | Cross-session vLLM telemetry & dispatch-dynamics forensics. |
| [`WSL_FINAL_SUPERVISORY_SESSION_AUDIT_2026-09-13.md`](audits/WSL_FINAL_SUPERVISORY_SESSION_AUDIT_2026-09-13.md) | WSL supervisory session audit (probe-loop diagnosis, wait-endpoint forensics). |
| [`AUDIT_MANIFEST_2026-09-14.md`](audits/AUDIT_MANIFEST_2026-09-14.md) | Findings manifest for the 2026-09-14 session (protocol amendments). |
| [`QWEN_EVO_AUDIT.md`](audits/QWEN_EVO_AUDIT.md) | Collaborative Evo lineage / peer-review stress analysis. |
| [`CASTOR_PROGRESS.md`](audits/CASTOR_PROGRESS.md) | Castor implementation progress ledger. |
| [`AUDIT_2026-09-27_MILESTONE_CLOSEOUT.md`](audits/AUDIT_2026-09-27_MILESTONE_CLOSEOUT.md) | Milestone closeout audit (suite-count sync, protocol lock). |
| [`qwen-usage-audit-2026-09.md`](audits/qwen-usage-audit-2026-09.md) | Cloud/local dispatch-balancing audit; empirical basis for the `CLAUDE.md`/`GEMINI.md` §3 dispatch policy. |

Raw telemetry captured during the 2026-09-13 cross-session analysis lives in
[`docs/audits/telemetry/`](audits/telemetry/).

---

## Archive (`docs/archive/`)

Long-form engineering notes superseded by the active specs:

- [`DEVELOPMENT_NOTES_2026.md`](archive/DEVELOPMENT_NOTES_2026.md) —
  Engineering-decisions and benchmark changelog (largely pre-Rust / JS-runtime).
- [`marathon-case-study-2026-09.md`](archive/marathon-case-study-2026-09.md) —
  Single-session revision-marathon case study.

---

## Related References

- **MCP Server Architecture**: [`src/mcp/mod.rs`](../src/mcp/mod.rs) & [`src/main.rs`](../src/main.rs)
- **Canonical Operating Contract**: [`AGENTS.md`](../AGENTS.md)
- **Benchmark Suites**: [`benchmarks/`](../benchmarks/) (SWE-bench validation, vLLM wedge-repro)
