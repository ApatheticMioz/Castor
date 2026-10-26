# Qwen Usage Audit — Balancing Cloud/Local Orchestration (2026-09)

**Scope:** Forensic cross-analysis of how cloud orchestrators (Antigravity/Gemini planner, Claude Code) drive the local Qwen3.8-27B execution harness (Castor, `mcp-qwen`), and how Castor serves them. Anchored on the 26-hour Antigravity session `38daaf48` (2026-09-11 08:36Z → 09-12 10:53Z, project `d:\Work\dataPecedes\ai-assistant`) whose opening instruction — *"use very scoped, small prompts to qwen — don't specify too much, but don't be too broad"* — was nonetheless followed by monolithic prompts, babysitting, and cancellations. Evidence: the session transcript (950 steps, 35 qwen calls), Castor telemetry (259 sessions, 5,520 assistant turns since 09-05), task-state files, MCP client logs, Antigravity crash markers, three comparison sessions, the serving recipe ([syv-ai/qwen38-27b-rtx3090](https://github.com/syv-ai/qwen38-27b-rtx3090)), and field research on effort economics.

**Outcome:** a balanced-usage architecture (§2), empirically grounded dispatch policy (§3, now codified in CLAUDE.md/GEMINI.md §3), Castor hardening fixes (§6), and a standing dispatch scorecard (`mcp-qwen/scripts/dispatch-scorecard.mjs`).

---

## 1. Verdict in one paragraph

The orchestrator **starts disciplined and degrades under pressure**. Phase A of the audited session (08:36–11:38Z) is textbook: three scoped investigations (398–782 chars) → plan → three milestone-split execution dispatches (856–1,369 chars). Under time pressure it relapses (3,119-char summary paste; 1,877-char feature-mega-prompt that died at exactly 100 turns and needed a 40-min remediation pass), and when corrected it **overshoots** to exact-code spoon-feeding rather than finding the middle. Meanwhile the engine was innocent all week — zero hangs, zero breaker trips — while **infra silently destroyed work**: an MCP instance death killed a healthy task mid-flow with no trace, and a 4.5 h retention sweeper deleted day-1 telemetry overnight. The balance problem is therefore three-sided: orchestrator discipline, infra traceability, and effort economics. All three get concrete fixes below.

## 2. The cost model and the architecture

```
wall-clock ≈ Σ per dispatch [ queue-wait   (MAX_SEQS=1: everything serializes)
                            + prefill      (prefix-cache hit ~0.6 s vs ~22 s cold @25k tok — 35×)
                            + rounds × ( thinking_tokens(effort) ÷ ~121 tok/s + tool time ) ]
           + rework        (remediation passes, cancels of healthy work, duplicate scope)
```

Measured anchors (serving recipe + our telemetry):

| Variable | Measured | Consequence |
|---|---|---|
| Decode rate | 121–133 tok/s single-stream (46 without speculation; 381 in quoting mode `DFLASH_TOKENS=15`) | Our observed 0.5–3.5 min/round at `xhigh` ≈ **6k–28k thinking tokens per round**. Effort is the dominant per-round cost; round count multiplies it. |
| Prefix cache | 0.6 s TTFT warm vs ~22 s cold (25k tok); 93.8% hit rate in production | Session cohesion is nearly free; session rolls pay full prefill. Roll only on milestone change. |
| Turn cost | `xhigh` deliberation 0.5–3.5 min between tool rounds; tool execution <1 s | Cost ≈ (questions per dispatch) × (rounds) × (effort latency). One artifact per dispatch. |
| Session decay | 21/125 multi-turn sessions show late-turn throughput decay; worst are 100-turn sessions | Cap working sessions at ~60–80 turns; the 100-turn sessions (`QWEN_MAX_TURNS`) are the worst performers. |
| Cloud effort economics | GPT-6 Astra cost frontier ≈$0.82/task @low → $3.26 @max; consensus = medium effort for cost | Quality-at-lower-effort dominates attempt-count costs on the cloud side too. |

**The architecture — two-sided asymmetric effort economics:**

- **Cloud at HIGH effort for decisions.** Routing, decomposition, prompt-shaping, kill/no-kill calls. The audit shows each orchestrator mistake costs 30–120 min of serialized engine time (one relapse ≈ 2 h); dollars of cloud reasoning are nothing against that. Never economize on the deciding side.
- **Local tiered by task class** (via the new per-dispatch `reasoning_effort` param; default `xhigh` unchanged): bounded mechanical implementation → `medium`; mutation-planning, security/correctness verification, tricky debugging → `xhigh`. Tier down consciously, per task class — never silently. (arXiv 2512.19585: reasoning gains plateau ~20k tokens and can dip at max budget — overthinking is real. Empirical correction during implementation: the **served chat template accepts exactly {xhigh, medium, low}** — `off` and `high` return 400, despite the documented family tier list; and the serving launcher sets an engine-side default of `medium` via template kwargs, which the harness's explicit `xhigh` always overrides today.)
- **Interface discipline joins the two** (§3): small, single-artifact, pointer-based dispatches into prefix-cache-coherent sessions, supervised telemetry-first.

## 3. Dispatch policy (empirical, codified in CLAUDE.md/GEMINI.md §3)

The session-level evidence behind each rule:

| Rule | Evidence |
|---|---|
| ≤1,500-char instruction; ONE artifact per dispatch | Every uncontested dispatch: 398–1,473 chars. Every failure ≥1,877 chars (or exact-code). Compound 3-artifact prompt → 54 tool calls / 57 min. |
| Pointers, not pasted content | The 3,119-char summary-paste was the caught monolith. Path + AST coordinate + invariants + acceptance criteria is the middle the oscillating orchestrator never found alone. |
| Session cohesion; roll on milestone change; ~60–80-turn cap | Prefix-cache economics (35× TTFT); decay table above; the 1,877-char monolith died at exactly 100 turns (`turn_limit_reached`) after 77 min. |
| T₀=50 m before first check-in; telemetry before ANY kill; never cancel healthy work | 4 status calls in 7 min; check-in 82 s post-dispatch; a healthy 36-min task (40 tool calls) was killed mid-flight at 19:42Z — 100% loss. Cancels destroyed ~46 min of engine work. |
| Duplicate-guard: verify against `~/.qwen/sessions/<id>/events.jsonl` before re-dispatch | The "duplicate dispatch" was a planner phantom: two `call_mcp_tool` transcript steps, ONE real task (server coalesced). Planner transcripts are intent ledgers, not ground truth. |
| Claim→verify slice pairs for corruption-class findings | This audit retracted 2 of its own findings via adversarial verify-slices (§5). Cheap insurance against pattern-match diagnoses. |
| Read-only slices: "return the report as your final message; write no files" | 3 of 7 read-only slices wrote stray files into the repo anyway under "READ-ONLY, no mutations" phrasing. |
| Scope tasks to finish ≤~60 turns | Keeps headroom under `QWEN_MAX_TURNS=100` and out of the decay zone. |
| Exemplar: session `b6ea6061` (09-10) | 7 dispatches, median 815 chars, 2 status calls total, zero errors, zero user friction. The spectrum (a113a65d: 5,180-char max + server-stop nuke; b40569e1: 36 polls/10 dispatches = babysitting) proves discipline — not model or tooling — is the differentiator. |

## 4. Orchestrator findings (audited session, chronological)

1. **Phase A — good discipline (09-11 08:36–11:38Z):** 3 scoped investigations → plan artifact → milestone-split executions F-1, F-2&3, F-4 → adversarial test. Matches policy exactly.
2. **First infra strike (10:12:47Z):** `pattern_test_s1` died externally mid-tool-flow (event stream stops, no `session_end`, no abort event, no orchestrator cancel): an MCP server instance death took the child runner's process tree. The orchestrator's "stale/dead" re-dispatch call was **correct but unexplainable** — the kill left no trace. (User's memory of "the MCP crashed as I opened another session" is real: Antigravity crash markers exist at 09-11 08:34:17Z — 2 min before this session started — and 09-12 10:55:03/12Z, bracketing a Claude Code MCP connect to the second; all markers are 0-byte files, so cause is unattributable — itself a defect, Antigravity-side.)
3. **Oscillation under correction (19:02–19:44Z):** 3,119-char monolith → user catch → cancelled at 2 min → over-correct to exact-file/code spoon-feeding → cancelled after 11 s → right-sized `attack_session_behavioral` → then **cancelled that healthy 36-min task** reacting to another user nudge → user: "WHAT PART OF DONT INTERRUPT DID YOU NOT UNDERSTAND."
4. **Day-2 relapse (09-12 08:48Z):** `whatsapp_context_slice1`, 1,877 chars, feature-sized → 77 min, exactly 100 turns, `turn_limit_reached`, isError → 2,255-char remediation (40 min, 111 calls). One feature ≈ 2 h engine time + rework that proper slicing avoids.
5. **Cross-session spectrum** (three comparison sessions, §3 table): under-polling + server-stop nukes (a113a65d) → babysitting + "model unreachable" (b40569e1) → clean minimalism (b6ea6061). The audited session sits at the noisy end.

## 5. Infra findings (Castor + Antigravity)

1. **Silent task death — traceability gap (fixed):** external process-tree kills leave no `session_error`, no task-state trace. Fix: kill/orphan event emission on runner death.
2. **Task telemetry self-destructs at 4.5 h (fixed):** `TASK_RETENTION_MS = DEFAULT_TIMEOUT_MS + 30 min` (`config.js:94`) — invariant "retention must outlive the longest legal task" doubles as "audit evidence evaporates overnight." The sweeper (`cleanOldTasks` → `listTasksFromDisk`, every 300 s + on every list/status/cancel) unlinks `*.json` older than 4.5 h by mtime. Fix: env-configurable retention, 7-day default.
3. **`.tmp_*` crash orphans accumulate forever (fixed):** the extension filter (`f.endsWith(".json")`) never matches `task_*.json.tmp_<pid>_<ts>` orphans from a `writeFileSync` whose `renameSync` never ran (failure swallowed by `catch {}` at `task_registry.js:50`). Fix: sweep them age-gated; log the swallowed rename failure.
4. **`promptTokens` never recorded (fixed):** 0 in 100% of 5,520 events — prompt-size telemetry was impossible (this audit's prompt↔TTFT correlation is undefined as a result). Fix: record it in session events.
5. **Engine exonerated:** 0 hangs, 0 circuit-breaker trips in-window; 66 `continuation_injected` + 27 `empty_stream_retry`, all recovered (the P2b/P2d machinery works); TTFT spikes cluster on specific days (09-05 cold-start cluster; one 521 s outlier on 09-08), not on prompt size — dominated by server-side stalls.
6. **~~result.text cross-task splice~~ RETRACTED:** byte-level verification shows the on-disk file clean; the earlier claim was a substring-grep false positive (`step_index` legitimately appears in both outputs). The write path is provably safe: single atomic `saveTaskToDisk` (tmp+rename), `result` assigned exactly once at the terminal write, per-task stream buffers, `MAX_CONCURRENT=1`. Methodological note: this is the same error class as the orchestrator's premature "stale/dead" — pattern-matched conclusion ahead of verification. The claim→verify protocol exists because of it.
7. **Doc/code drifts (reconciled, `2449b84`):** `QWEN_RACE_MS` 15 s code vs 45 s docs; `QWEN_REASONING_EFFORT` docs said "(unset)" but code default is `xhigh`; `QWEN_MAX_CONCURRENT` row stale after the semaphore raise; `FIRST_TOKEN_TIMEOUT_MS` documented as a live kill mechanism but consumer-less — now documented as a reserved knob (retained per the honesty-drift D13 decision) with the stream-idle first-byte watchdog named as the live protection. `SLOT_WEDGED_MS` remains an inert import in `semaphore.js` (unused export; left in place).
8. **Harness prompt overhead:** every dispatch carries ~1,331 chars of injected operational directives (1,428-char instruction → 2,914-char delivered message, ~48% boilerplate). Reordered constant-prefix-first for prefix-cache friendliness.

## 6. Changes shipped with this audit

**Protocol (repo `CLAUDE.md` / `GEMINI.md`, §3 + §4.2; globals symlink to these):** the §3 policy table above, distilled into the dispatch contract; §4.2 supervision hardened with telemetry-before-kill and the healthy-work cancellation ban. Tool description of `qwen_coworker` mirrors the policy and documents the new parameter.

**Castor code (`mcp-qwen`), one concern per slice:**
1. `reasoning_effort` per-dispatch parameter on `qwen_coworker` (`ac212be`; tiers corrected against the live template in `f956c00` — engine accepts exactly {xhigh, medium, low}).
2. Retention: `TASK_RETENTION_MS` env-configurable, default 7 days (`c0ebd32`).
3. `.tmp_*` orphan sweep (age-gated) + rename-failure logging (`c0ebd32`).
4. `promptTokens` recorded in session events (`263c7b3`): requests send `stream_options.include_usage`; real `prompt_tokens` land in metrics, chars-based fallback flagged `promptTokensEstimated`.
5. Kill/orphan event emission so external deaths leave a trace: `markTaskOrphanedOnDisk` appends exactly one `{session_error, reason: orphaned}` event, double-terminal-guarded, cross-instance-safe (`2388236`).
6. Doc drift reconciliation (`2449b84`) + constant-prefix prompt ordering.
7. Concurrency raise: engine `MAX_SEQS` 1 → 2 with the harness semaphore kept 1:1, boot deadline 180 s → 480 s for the 2-seat CUDA-graph boot, single-slot test suites pinned, launcher records the full 8 → 1 → 2 trajectory with deadlock watch-fors (`b352f8b`).

> [Warning] True 2-job parallelism requires an MCP server restart: long-lived servers serve boot-time code (the running servers still enqueue at 1 seat). The engine side is live; the harness side activates on next server start.

> [Warning] Live-dispatch verification of (1) requires an MCP server restart (long-lived servers serve boot-time code). Unit tests cover the plumbing; restart before trusting `medium`-effort dispatches in production.

**Scorecard:** `mcp-qwen/scripts/dispatch-scorecard.mjs` — reads `~/.qwen/sessions/*/events.jsonl` + `tasks/*.json`, emits per-orchestrator: prompt-size distribution, turns/task, cancels, babysit events (status/cancel <120 s post-dispatch), continuation/retry counts, decay flags. Run it after any heavy orchestration day to check balance compliance.

**Deleted after absorption:** `qwen38_local_tool_ledger.md`, `castor_knob_inventory.md`, `mcp-qwen/docs/task-state-persistence-analysis.md` (raw working ledgers; this document is the synthesis).

## 7. Open items

1. **Effort-tier experiment (before hardening `medium` as policy):** paired bounded-implementation slices, `medium` vs `xhigh`; compare rounds, wall-clock, outcome quality. The Kaitchup 27B effort benchmarks are non-agentic (and paywalled); our own agentic numbers must come first.
2. **Quoting-mode experiment:** extraction/report slices under `DFLASH_TOKENS=15` (381 tok/s on verbatim-copy workloads; costs half the seats + 8k context) — engine-side launcher change, separate decision.
3. **Antigravity-side defects (out of this repo's reach):** 0-byte crash markers (crash handler writes nothing); planner transcript phantom-duplicate steps; 3 planner `invalid tool call (invalid_args)` parse errors in the audited session alone.
4. ~~Effort values to verify engine-side~~ **RESOLVED**: the served chat template validates `reasoning_effort` to exactly {xhigh, medium, low} (400 on anything else); the slice-1 schema encodes the verified set (`REASONING_EFFORT_TIERS`).

## Appendix A — Knob classification (decision-relevant subset)

| Class | Knobs |
|---|---|
| **Per-dispatch (tool params)** | `timeout_ms` (4 h default, 10-min floor), `session_id`, `cwd`, `extensions[]`, `skills[]`, evo params, **new:** `reasoning_effort` |
| **Per-dispatch via call-time env read** | `QWEN_REASONING_EFFORT` (`getReasoningEffort()` inside each `streamChat`) — now superseded by the param |
| **Global config (restart to change)** | `QWEN_MAX_TOKENS` 49,152/turn · `QWEN_MAX_REASONING_TOKENS` 32,768/turn · `QWEN_MAX_TURNS` →100 · `QWEN_MAX_CONTINUATION_TURNS` 8 · `QWEN_EMPTY_STREAM_RETRIES` 2 · stream idle 15 min · inactivity 30 min · retention (now 7 d) · `MAX_CONCURRENT` 1 · wedge/heal/lock constants · ports |
| **Engine-side (external launcher)** | `max_model_len` 245,760 · `MAX_SEQS=1` · DFlash2 (7 draft positions, 53–79% acceptance) · KVarN k4v2 · prefix cache · `--reasoning-parser qwen3` |

Context growth has **no compaction**: 32 KB/1,000-line tool-output truncation, the 245K `ContextExhaustedError` fail-fast, and protocol-level session rolls are the only levers.

## Appendix B — Audited-session dispatch ledger (condensed)

35 qwen calls total: 24 `qwen_coworker` dispatches, 10 `qwen_task` (status/list/cancel), 1 `qwen_server` health check. Full table preserved in the session transcript (`brain\38daaf48…\logs\transcript_full.jsonl`); key rows:

| When (Z) | Dispatch | Chars | Outcome |
|---|---|---|---|
| 09-11 08:36–08:47 | cron/pattern investigation ×3 | 398–782 | clean |
| 09-11 09:01–09:46 | F-1 / F-2&3 / F-4 executions | 856–1,369 | clean, milestone-split |
| 09-11 10:07 | pattern_test_s1 | 1,171 | killed externally 10:12:47Z (infra) |
| 09-11 10:15 | pattern_test_s2 | 778 | completed 6.4 min (proves s1 was healthy) |
| 09-11 11:38 | git_commit_push_s1 | 1,154 | clean |
| 09-11 19:02 | session-summary paste | **3,119** | cancelled 2 min (user-caught monolith) |
| 09-11 19:04 | exact-file spoon-feed | 1,092 | cancelled 11 s (over-correction) |
| 09-11 19:06 | attack_session_behavioral | 1,033 | **killed healthy at 36 min** (19:42Z) |
| 09-11 19:44–22:16 | phase1/2/3 attack+remediation | 1,142–1,473 | clean (incl. the "phantom duplicate" at 20:48/49 — one real task) |
| 09-12 03:29–03:37 | investigations ×2 | 1,218–1,384 | clean |
| 09-12 08:48 | whatsapp_context_slice1 | **1,877** | **100 turns, turn_limit_reached, isError** |
| 09-12 10:07 | whatsapp_context_remediation | **2,255** | completed 40 min (rework pass) |
