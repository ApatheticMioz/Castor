# scripts/

Repo-level tooling, launchers, and utility scripts that live outside the
core Rust crate. These operate on the Castor harness's on-disk telemetry,
WSL launchers, and development workflows.

## qwen_tasks_analysis.mjs

Telemetry exporter for Qwen task dispatches. Exports per-dispatch telemetry rows
from on-disk session ledgers and task records into structured JSON.

Node >= 22, ESM, **no dependencies**.

### Inputs

| Flag             | Meaning                                                                 |
|------------------|-------------------------------------------------------------------------|
| `--tasks-dir`    | Directory of per-dispatch task files: `task_<id>.json` (`id`, `sessionId`, `cwd`, `prompt`, `createdAt`, `startedAt`, `finishedAt`, `status`, `done`, `isError`, `reasoningEffort`, `toolCallsCount`, `fileOps`, …) |
| `--sessions-dir` | Directory of per-session event logs: `<sessionId>/events.jsonl` (per-turn `assistant_message` events with `metrics{promptTokens, ttftMs, totalMs, reasoningTokens, completionTokens, tokensPerSec}`, plus `tool_call` / `continuation_injected` / `empty_stream_retry` / `engine_empty_response` / `session_end` events) |
| `--out`          | Output JSON file (array of per-dispatch rows)                            |

### Usage

```sh
node scripts/qwen_tasks_analysis.mjs \
  --tasks-dir    ~/.castor/tasks \
  --sessions-dir ~/.castor/sessions \
  --out          qwen_tasks_analysis.json
```

A readable summary (dispatch/session counts, reused sessions, null-`promptTokens`
count, output path) is written to **stderr**; the JSON array goes to `--out`.

### Output schema

Standard dispatch telemetry schema — **25 fields, fixed order**:

```
taskId, sessionId, cwd, createdAt, durationSec, status, isError,
endStatus, promptLen, prompt, toolCallsCount, tools, totalBash,
bashCommandsSummary, filesWritten, filesReadCount, turns, promptTokens,
completionTokens, thinkingTokens, ttftMedMs, rateAvgTokS, continuations,
emptyStreamRetries, engineEmptyResponses
```

### Metrics Semantics

1. **`promptTokens` = MAX per-turn `metrics.promptTokens`** across the
   session's events — representing the final context depth. When the engine
   does not report a non-zero `promptTokens` (or if no events are recorded)
   the value is **`null`**, distinguishing unknown from zero.

2. **Session-cumulative fields are stamped ONLY on the LAST dispatch row** of
   each session (the most recent `createdAt`); every other row of that session
   carries **`null`**. The cumulative fields are: `turns`, `completionTokens`,
   `thinkingTokens`, `tools`, `totalBash`, `ttftMedMs`, `continuations`,
   `emptyStreamRetries`, `engineEmptyResponses`. This ensures aggregation
   queries do not multiply values across multi-turn sessions.

   Session-level *descriptive* fields that are **not** cumulative sums —
   `endStatus`, `bashCommandsSummary`, `filesWritten`, `filesReadCount`,
   `promptTokens`, `rateAvgTokS` — are present on **every** row of the session
   (they describe session state/depth, so they do not double-count).

3. **`rateAvgTokS` uses an honest derivation.** The mean per-turn tokens/sec
   uses the engine's `metrics.tokensPerSec` when it is present and > 0; when
   the engine field is absent or 0, the rate is **derived** as
   `completionTokens / ((totalMs - ttftMs) / 1000)` — i.e. generation time
   only, excluding time-to-first-token. The output schema has no dedicated
   "derived" flag, so this derivation is documented here (and in the
   exporter's header) rather than encoded in a field.

### Verification

Telemetry parsing, statistics derivation, and event ledger operations are verified
as part of the authoritative test gate:

```sh
cargo test telemetry
```
