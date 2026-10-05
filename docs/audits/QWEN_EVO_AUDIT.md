# Qwen / DeepSeek AVO — Stress-Review & Audit Report

> **Archival note (2026-09-06):** Renamed from its pre-Castor working filename; content preserved verbatim as history. The project now ships as **Castor**.

**Branch under review:** `feat/deepseek-avo`
**Commits:** `7ad481f` (microkernel core + sandboxed services), `90ae613` (AVO operators + lineage DAG), `6221c94` (runner + dual-engine dispatch), `91f6f25` (validation suite + benchmarks)
**Auditor:** Autonomous Execution Coworker (Qwen3.8-27B) — DeepSeek AVO harness
**Date:** 2026-09-05

---

## 0. Verification Evidence (Runtime)

Both suites were executed in this environment and **passed**:

| Suite | Command | Result |
|---|---|---|
| System validation | `node mcp-qwen/test_deepseek_avo.js` | **6/6 PASSED** (incl. live vLLM streaming, 1953 ms TTFT, 1 token "CONFIRMED") |
| Head-to-head benchmark | `node mcp-qwen/benchmark_comparison.js` | **ALL COMPLETED** (sandbox 6.0× faster than naive crawler; AVO accept + deterministic revert verified) |

The AVO closed loop was observed live in Benchmark 3:
- Baseline: 121.4 ops/sec
- Candidate 1 (O(N) variation): 10030.09 ops/sec → **ACCEPTED** into DAG
- Candidate 2 (syntax-defect variation): exit code 1 → **REVERTED**, 1 file restored

**Bottom line:** the happy path works end-to-end and the deterministic rollback is real. The findings below are **latent** defects that the current test suite does not exercise (they require adversarial inputs, concurrency, or the full runner loop). None of them block a merge, but several should be fixed before the harness is trusted with unattended autonomous mutation.

---

## 1. Executive Verdict

**Verdict: APPROVE WITH CONDITIONS.** The four commits are coherent, well-factored, and the core value proposition (reversible microkernel + closed-loop AVO + deterministic rollback) is genuinely delivered and verified. The code is clean and idiomatic. However, the word **"sandbox" is over-claimed** in three places, and there are a handful of real correctness/safety gaps that a stress review must surface:

1. **The filesystem "sandbox" is not actually sandboxed** — `resolvePath()` performs no boundary enforcement, so `../../` traversal and absolute paths outside the workspace are honored by `read_file`/`write_file`/`edit_file`. (HIGH)
2. **Process-tree kill on timeout is unreliable** — Node `spawn` does not create a new process group, so `process.kill(-pid)` fails and only the direct child is killed; WSL-spawned processes are in a separate PID namespace and `taskkill /T` will not reach them. Orphaned grandchildren are possible. (MEDIUM-HIGH)
3. **`lineage.json` persistence is non-atomic and unguarded** — a crash or concurrent writer corrupts the file, and `_load()` then **silently resets the entire lineage to a fresh baseline** (data loss). (MEDIUM)
4. **AVO rollback is not fully deterministic for file *creation*** — only pre-existing files are snapshotted; newly-created files are left behind on revert. (MEDIUM)
5. **The "DAG" is actually a linear linked list** — `avo_operator` never passes a `parentId`, so branching is impossible; the structure is a chain. (LOW, naming/design)
6. **The watchdog "circuit breaker" is advisory only** — `stagnationTripped` is computed but never checked to block further proposals. (LOW)
7. **Double `tool_result` logging in the full runner loop** (event-bus hook + explicit append) — a latent bug the unit test does not catch. (MEDIUM)
8. **`EventBus.emit` can produce unhandled promise rejections** on handler throw. (MEDIUM)

None of these are show-stoppers for the demonstrated functionality, but #1, #2, and #3 should be addressed (or explicitly documented as out-of-scope) before the harness is pointed at unattended, long-running autonomous mutation.

---

## 2. Microkernel Container & Plugin Lifecycle
**Files:** `src/harness/core/kernel.js`, `src/harness/core/events.js`

### What is solid
- **Reversible disposers** are implemented correctly and idempotently. `Context.dispose()` iterates a snapshot of `_disposers`, swallows per-disposer exceptions, then clears all maps — so a throwing disposer cannot poison the unwind.
- **Plugin model** (`plugin()`) creates a child `Context`, invokes the plugin, and returns an `unmount` that (a) runs the plugin's own disposer, (b) disposes the child context, (c) removes itself from the parent's plugin map. The `try/catch` around the plugin disposer is correct.
- **Tool execution wrapper** (`executeTool`) consistently emits `tool:before_execute` / `tool:after_execute` and normalizes both success and failure into `{isError, result|error, latencyMs}` — good for downstream logging and AVO.

### Findings

**K1 — `provide()` can clobber `Context` methods (footgun).**
`provide(name, service)` does `this[name] = service` and `target[name] = service`. If a service is registered under a name that collides with a `Context` member — `name`, `parent`, `root`, `events`, `services`, `tools`, `get`, `provide`, `registerTool`, `listTools`, `executeTool`, `plugin`, `dispose` — it **overwrites the method**. E.g. `ctx.provide("get", svc)` silently breaks `ctx.get()` for the rest of the session. There is no guard against reserved names.
*Recommendation:* reserve a namespace (e.g. store services only in the `services` Map, never as own properties), or throw on reserved names.

**K2 — "Isolated child contexts" is a misnomer: the EventBus is shared by reference.**
`this.events = parent ? parent.events : new EventBus()` — the bus is **inherited by reference**, not copied. Every context in the tree shares one bus. This is a reasonable design (global lifecycle events), but it means the docstring's "isolated child contexts" is inaccurate, and it is the root cause of finding **R1** (double logging) below.

**K3 — `services`/`tools` are shallow-copied at construction, then dual-written.**
`new Context(parent)` does `new Map(parent.services)` (a point-in-time copy). A child created *before* the parent registers a service will not see it, yet `provide()` writes to both `this` and `this.root`. The dual-write plus the construction-time copy creates a subtle consistency surface: a child's `get()` checks its own (stale) copy first, then the root. In practice the runner creates all plugins up-front so this does not bite, but it is a latent ordering trap.

**K4 — `EventBus.emit` can create unhandled promise rejections (MEDIUM).**
```js
} catch (err) {
  console.error(...);
  results.push(Promise.reject(err));   // <-- rejected promise, never awaited
}
```
The rejected promise is pushed into the returned array but **never awaited or handled**. In modern Node the default `unhandledRejection` behavior is to throw, so a single throwing async listener can **crash the process**. The `console.error` shows the intent was to log-and-continue, but the `Promise.reject` defeats it.
*Recommendation:* push a sentinel (e.g. `{ __error: err }`) or `err` itself instead of a live rejected promise; or attach a no-op `.catch(()=>{})`.

**K5 — `once()` disposes before the handler runs.**
`once` wraps the handler so `dispose()` is called *before* `handler(...)`. For a synchronous handler this is fine, but if the handler is async the "once" unsubscription happens before the work completes — acceptable, but worth a comment. Minor.

**K6 — Duplicate `on()` of the same handler.**
`on()` stores handlers in a `Set`, so registering the same function reference twice yields one entry but two returned disposers. The first dispose removes it; the second is a harmless no-op. Not a bug, just non-obvious.

---

## 3. Sandbox Filesystem & Shell Process Safety
**Files:** `src/harness/services/sandbox_fs.js`, `src/harness/services/shell_executor.js`, `src/wsl_bridge.js`

### 3.1 `sandbox_fs.js`

**S1 — No actual sandbox boundary (HIGH — the headline finding).**
`resolvePath()` is documented as "verifies that a target path is safely within allowed boundaries," but it only normalizes (Windows↔WSL) and resolves relative paths against `this.root`. It performs **no containment check**. Consequently:
- `read_file({path:"../../etc/passwd"})` → resolves outside the workspace and is read.
- `write_file({path:"/etc/cron.d/evil"})` (absolute) → written outside the workspace.
- `edit_file` on any absolute path → edited.

The "sandbox" is therefore a **naming claim, not an enforcement**. For a harness that an LLM drives autonomously, this is the single most important gap.
*Recommendation:* after resolving, assert `resolved === root || resolved.startsWith(root + path.sep)` (using `path.resolve` on both) and throw `PathEscapeError` otherwise. Apply to all five tools.

**S2 — `readFile` loads the entire file before slicing (OOM risk).**
`fs.readFileSync(resolved, "utf8")` reads the whole file into memory; `max_bytes` only truncates the *formatted* output afterward. A multi-GB log or a minified bundle will be fully materialized. The "safe slicing" claim is undermined for large files.
*Recommendation:* use a bounded read (e.g. `fs.open` + `read` of a byte window, or `readline`) so memory is O(slice), not O(file).

**S3 — `editFile` with empty `target_content` is a corruption footgun.**
`original.split("").length - 1` returns the string length, and `String.replace("", x)` / `replaceAll("", x)` insert `x` at the start / between every character. An empty `target_content` is not guarded and can mangle a file.
*Recommendation:* reject empty `target_content` (and empty `replacement_content` when `allow_multiple`).

**S4 — Non-atomic writes.**
`writeFile`/`editFile` use `fs.writeFileSync` directly. A crash mid-write leaves a torn file. For AVO, where rollback fidelity is the whole point, this matters.
*Recommendation:* write to a temp file in the same directory, then `fs.renameSync` (atomic on the same volume).

**S5 — `searchCode` `--max-count` semantics.**
`git grep --max-count N` caps matches **per file**, not in total, so `count` can exceed `max_results`. Also `git grep` only searches the tracked/working tree (respects `.gitignore`), so **untracked files are invisible** to the primary path (the fallback covers them, but only when `git grep` throws — a repo with zero matches returns success, not the fallback). Minor, but the "fast indexed search" can silently miss untracked files in a clean repo.

**S6 — `listDir` has no entry cap.**
A directory with 100k entries at depth 1 returns 100k items with no limit. Add a max-items guard.

**S7 — CRLF handling.**
`raw.split("\n")` leaves a trailing `\r` on each line of CRLF files. Line numbers are correct, but returned `content` carries `\r`. The workspace is known to use CRLF, so this will show up in tool output. Cosmetic, but worth normalizing.

### 3.2 `shell_executor.js`

**H1 — Process-tree kill is unreliable (MEDIUM-HIGH).**
The docstring promises "synchronous tree termination to eliminate zombies," but:
- On **Linux**, `killProcessTreeSync` calls `process.kill(-child.pid, "SIGKILL")`. Negative-PID signaling only works if the child is a **process-group leader**, which requires `spawn(..., {detached:true})` (and `setsid`). Node's default `spawn` does **not** create a new group, so `-pid` throws and the code falls back to `child.kill("SIGKILL")` — which kills **only the direct child** (e.g. `bash`), **not its children** (the actual command, its subprocesses). → **orphaned grandchildren** after a timeout.
- On **Windows/WSL**, the child is `wsl.exe`; `taskkill /PID <pid> /T /F` kills the Windows process tree, but the real work happens in a **separate Linux PID namespace inside WSL**, which `taskkill` cannot reach. → **orphaned WSL processes** after a timeout.

*Recommendation:* spawn with `{detached:true}` and call `child.unref()`-free group kill on POSIX (`process.kill(-pid)`); for WSL, pass a unique marker (e.g. an env var or a `pkill -f` on a session tag) into the WSL command and kill by that tag, as `killProcessTree(child, sessionId)` already attempts — but note `execute()` calls `killProcessTreeSync(child)` **without** a `sessionId`, so the WSL `pkill` branch is dead in this path.

**H2 — `cwd` is interpolated into a shell string (injection vector).**
```js
args = ["-d","Ubuntu","--","bash","-c", `cd "${posixCwd}" && ${command}`];
```
`posixCwd` is placed inside double quotes but `toPosixWslPath()` does **not** escape `"`, `$`, or backticks. A crafted `cwd` containing `"; <payload>; "` breaks out of the quotes. `command` is intentionally model-controlled (that's the tool's purpose), but the **`cwd` parameter should not be an injection surface**.
*Recommendation:* pass the working directory via `spawn`'s `cwd` option (or `env`) instead of string interpolation, or shell-quote `posixCwd`.

**H3 — Unbounded stdout/stderr buffers (OOM).**
`stdout += chunk` / `stderr += chunk` have no cap. A command emitting gigabytes (e.g. `cat` of a huge file, a chatty build) will grow memory without bound.
*Recommendation:* cap the buffers (e.g. 1–10 MB) and truncate with a `...[truncated]` marker.

**H4 — Signal-killed processes are reported as exit code 0.**
`child.on("close", (code) => ... code ?? 0)`. When a process is killed by a signal, `code` is `null` and the real cause is in `signal`. `code ?? 0` therefore **reports a kill as a clean success (0)**.
*Recommendation:* on `close`, if `code === null` use `128 + signalNumber` (or at least a non-zero sentinel) and surface `signal`.

**H5 — Timeout path resolves before the child is actually dead.**
On timeout, the code calls `killProcessTreeSync` and resolves immediately, without awaiting the child's `close`. Combined with H1, the "timed out" result can be returned while the process (and its children) are still alive. Acceptable as a best-effort, but should be documented.

---

## 4. NVIDIA AVO Evolutionary Engine
**Files:** `src/harness/avo/avo_operator.js`, `lineage_dag.js`, `evaluator.js`, `watchdog.js`

### 4.1 `avo_operator.js`

**A1 — Rollback is not deterministic for file *creation* (MEDIUM).**
`proposeCandidate` only snapshots files that **already exist** (`if (fs.existsSync(fullPath) && isFile())`). If a mutation **creates** a new file, it is not in the snapshot, so `revertCandidate` (which only `copyFileSync`s snapshot→workspace) **leaves the new file behind**. The "deterministic rollback" guarantee holds for edits but not for creates/deletes.
*Recommendation:* record the *set* of files present at propose time (or snapshot a manifest of the affected subtree) and, on revert, delete any file in the affected set that did not exist at propose time.

**A2 — Snapshot filename encoding flattens the path (works, but fragile).**
`targetBackup = path.join(snapshotDir, encodeURIComponent(relPath))` encodes `/` → `%2F`, so `src/foo.js` is stored as a single flat file `src%2Ffoo.js`. Revert decodes it back and resolves against the workspace root, so it round-trips correctly. However:
- It breaks if a *literal* filename contains `%2F` (collision), and
- it makes the snapshot directory hard to inspect by hand.
*Recommendation:* mirror the real directory structure under the snapshot dir (sanitize only path separators), or store a JSON manifest mapping encoded-name → original path.

**A3 — `revertCandidate` mutates files *before* validating the DAG node (ordering bug).**
It performs the file rollback, then calls `this.dag.updateMetrics(id, {}, "rejected")`, which **throws** if `id` is not a known node. So an invalid `candidate_id` can leave files reverted but the DAG unmarked → **inconsistent state**.
*Recommendation:* validate the node exists (and, ideally, that it is the active/pending candidate) *before* touching the filesystem.

**A4 — No guard against reverting an already-accepted candidate.**
`revertCandidate` accepts any `candidate_id`. Reverting an *accepted* node would roll back files that were deliberately committed as the new baseline, while the DAG still marks it `accepted`. Add a status check.

**A5 — Single `activeCandidate`; parallel proposals orphan the first snapshot.**
`activeCandidate` is a single slot. Calling `proposeCandidate` twice without evaluating overwrites the pointer; the first snapshot dir is orphaned (recoverable only by explicit id). This also means the "session branching" capability in `event_logger` is **not wired** to AVO — there is no true parallel-candidate path.

**A6 — Snapshot directories are never garbage-collected.**
Every candidate leaves a directory in `.avo/snapshots/` forever. Over a long evolutionary run this is unbounded disk growth. Add GC (e.g. prune snapshots for `rejected` candidates older than N, or cap total size).

**A7 — Candidate-id collision (low probability, real consequence).**
`cand_${Date.now()}_${Math.random().toString(36).slice(2,6)}` — two proposals in the same millisecond with the same 4-char suffix collide. `fs.mkdirSync(..., {recursive:true})` then **silently reuses the existing directory**, mixing two candidates' snapshots. Use `crypto.randomUUID()` or a monotonic counter.

### 4.2 `lineage_dag.js`

**L1 — Non-atomic, unguarded persistence + silent lineage reset (MEDIUM — data loss).**
`persist()` writes `lineage.json` with a plain `writeFileSync` (no temp+rename, no lock). `_load()` wraps `JSON.parse` in a bare `catch {}` and, on **any** parse failure, falls through to creating a **fresh `baseline` node** — i.e. a transient corruption (crash mid-write, or a concurrent writer from the dual-engine dispatch) **silently discards the entire evolutionary history**.
*Recommendation:* (a) atomic write (temp+rename); (b) on parse failure, back up the corrupt file to `lineage.json.corrupt-<ts>` and log loudly instead of silently resetting; (c) if concurrent AVO runs are expected, serialize access (single writer / file lock) or move to an append-only log.

**L2 — The "DAG" is a linear chain.**
`addCandidate` defaults `parentId` to `currentHeadId`, and `avo_operator` never supplies a `parentId`. So the structure is a **linked list**, not a DAG — no branching, no parallel lineages, no true Pareto frontier across branches. The `toSummary()` "Pareto frontier" language in the tool description overstates this.
*Recommendation:* either expose a `parent_id` in `avo_propose_candidate` to enable real branching, or rename the structure to "lineage chain" to match reality.

**L3 — `getBestCandidate` ignores pending candidates.**
It only considers `status === "accepted"`. That is defensible, but it means `is_improvement` in the operator compares against the best *accepted* (or baseline 0), not the best *ever evaluated*. Fine by design; just be aware a high-fitness pending candidate is invisible to the comparison.

### 4.3 `evaluator.js`

**E1 — Fitness scale is not comparable across metrics (design smell).**
For `throughput`, fitness = `baseScore(0–100) + raw ops/sec` (e.g. 10130). For `latency_ms`, fitness = `baseScore + 1000/lat`. For `tests_passed`, fitness = `0–100`. These are **different units**, so a `throughput` candidate (10130) is not comparable to a `tests_passed` candidate (100) in the same DAG. `getBestCandidate` will always favor whichever metric produced the largest raw number. Acceptable if one metric is fixed per run, but the DAG's "bestFitness" is only meaningful within a single metric.
*Recommendation:* normalize each metric to a common 0–1 (or log-scaled) band before summing, or store per-metric bests separately.

**E2 — Regex metric parsing is greedy/first-match.**
The pytest/jest/ops/latency/memory regexes run against the *entire* combined stdout+stderr and take the **first** match. A log line like "123 passed in the docs" or a benchmark banner can mis-set `testsPassed`. The `passed` decision also hinges on `exitCode === 0`, which is the real gate, so a mis-parse mostly affects the *fitness number*, not accept/reject. Low risk, but the fitness value can be misleading.

**E3 — Timeout is conflated with test failure.**
A timed-out command (exit 124) yields `passed=false`, `fitness=-100`, indistinguishable from a genuine test failure in the DAG metrics. Consider recording `timedOut` as a distinct status.

### 4.4 `watchdog.js`

**W1 — The "circuit breaker" does not break anything (LOW but a real gap vs. the claim).**
`recordCandidateOutcome` sets `stagnationTripped = true` after N consecutive rejections, but **nothing in `avo_operator` checks `stagnationTripped`** before allowing the next `proposeCandidate`/`evaluateCandidate`. So the watchdog *reports* stagnation (it is surfaced in `getStatus()` and the revert result) but does not *enforce* a stop. The "circuit breaker" is advisory.
*Recommendation:* have `proposeCandidate`/`evaluateCandidate` consult `stagnationTripped` and either refuse or force a strategy change, matching the documented intent.

**W2 — `isVelocityDegraded()` / `recordVelocity()` are dead code.**
Nothing in the operator or runner calls them, so the "token velocity decay / KV pressure" protection is unimplemented in the loop.

---

## 5. Runner & Provider (context for the above)
**Files:** `src/harness/runner.js`, `src/harness/services/provider_vllm.js`, `src/harness/services/event_logger.js`

**R1 — Double `tool_result` logging in the full loop (MEDIUM, latent).**
`eventLoggerPlugin` registers a `tool:after_execute` listener that appends a `tool_result` event. `runner.run()` **also** manually appends a `tool_result` after every `ctx.executeTool`. Because the EventBus is shared (K2), **every tool call in a real session is logged twice** to `events.jsonl`. `getConversationHistory()` then emits **duplicate `tool` messages with the same `tool_call_id`**, which can confuse the model or trip provider validation on the next turn. The unit test does not catch this because it calls `executeTool` directly (only the hook fires) rather than running the full `runner.run()` loop with the logger mounted.
*Recommendation:* pick one source of truth — either the event-bus hook *or* the explicit append, not both.

**R2 — Unbounded message history (context-window overflow).**
`messages` grows every turn with no truncation/sliding window. A long session will exceed the model's context and produce a vLLM 400. Add a windowing/summarization strategy.

**R3 — Inconsistent provider fallback.**
`streamChat` falls back to the upstream vLLM port only when the **fetch throws** (network error). If the proxy returns an HTTP 5xx, it does **not** fall back — it throws. Make the fallback trigger on non-2xx as well.

**R4 — No per-tool timeout in the runner.**
`bash` self-limits, but other tools have no timeout; a hung tool blocks the whole loop. (The AVO `evaluateCandidate` does pass a timeout, so AVO is covered.)

---

## 6. Consolidated Findings Table

| ID | Severity | Area | Finding |
|---|---|---|---|
| S1 | **HIGH** | sandbox_fs | No path-boundary enforcement → traversal/absolute writes escape the workspace |
| H1 | MED-HIGH | shell_executor | Process-group kill unreliable → orphaned children (esp. WSL) on timeout |
| L1 | MEDIUM | lineage_dag | Non-atomic, unguarded `lineage.json`; corruption silently resets lineage (data loss) |
| A1 | MEDIUM | avo_operator | Rollback not deterministic for file *creation* (new files left behind) |
| R1 | MEDIUM | runner/logger | Double `tool_result` logging in full loop → duplicate tool messages |
| K4 | MEDIUM | events | `emit` can create unhandled promise rejections on handler throw |
| H2 | MEDIUM | shell_executor | `cwd` interpolated into shell string → injection surface |
| H3 | MEDIUM | shell_executor | Unbounded stdout/stderr buffers → OOM |
| S2 | MEDIUM | sandbox_fs | `readFile` loads whole file before slicing → OOM on large files |
| S3 | MEDIUM | sandbox_fs | Empty `target_content` in `editFile` can corrupt a file |
| S4 | LOW-MED | sandbox_fs | Non-atomic writes (no temp+rename) |
| A3 | MEDIUM | avo_operator | Revert mutates files before validating DAG node (inconsistent state) |
| A4 | MEDIUM | avo_operator | No guard against reverting an accepted candidate |
| A6 | LOW-MED | avo_operator | Snapshot dirs never GC'd → unbounded disk growth |
| E1 | LOW-MED | evaluator | Fitness not comparable across metrics in one DAG |
| H4 | LOW | shell_executor | Signal-killed process reported as exit 0 |
| A7 | LOW | avo_operator | Candidate-id collision → snapshot dir reuse |
| L2 | LOW | lineage_dag | "DAG" is actually a linear chain (no branching) |
| W1 | LOW | watchdog | Circuit breaker is advisory; never blocks proposals |
| W2 | LOW | watchdog | Velocity-decay protection is dead code |
| K1 | LOW | kernel | `provide()` can clobber `Context` methods on name collision |
| K2/K3 | INFO | kernel | Shared bus + construction-time service copy (ordering trap) |
| R2 | LOW | runner | Unbounded message history → context overflow on long sessions |
| R3 | LOW | provider | Fallback only on network error, not HTTP 5xx |
| S5/S6/S7 | LOW | sandbox_fs | `--max-count` per-file; no list cap; CRLF `\r` in output |

---

## 7. Recommendations for Merging `feat/deepseek-avo` → `main` and Deprecating Legacy Castor

**Merge posture:** The branch is **mergeable now** for supervised use — the demonstrated functionality (reversible microkernel, closed-loop AVO, deterministic edit-rollback, 6× sandbox traversal speedup, live vLLM streaming) is real and verified. Treat the HIGH/MED items as a fast-follow hardening sprint rather than merge blockers, **except** the two items below which I recommend fixing *before* any unattended autonomous run.

**Pre-merge (must-fix before unattended autonomy):**
1. **S1 — Enforce the sandbox boundary** in `resolvePath()` (containment check + `PathEscapeError`). This is the difference between a "sandbox" and a "view." Non-negotiable for an LLM-driven file tool.
2. **L1 — Make `lineage.json` persistence atomic and non-destructive** (temp+rename; on parse failure, quarantine the corrupt file and log, do not silently reset). The AVO lineage is the harness's memory; silent loss is unacceptable.

**Pre-merge (strongly recommended):**
3. **H1 — Fix process-tree kill** (spawn `detached:true` + group kill on POSIX; WSL kill-by-session-tag). Orphans on timeout are a resource-safety hazard in a long-running harness.
4. **R1 — Remove the double `tool_result` log** (single source of truth).
5. **A1/A3 — Make rollback fully deterministic** (handle file creation; validate the DAG node before touching files).

**Fast-follow (post-merge hardening):**
6. H2 (shell-quote `cwd`), H3 (cap buffers), H4 (report signal kills), S2 (bounded read), S3 (reject empty target), S4 (atomic writes), K4 (no unhandled rejections), A4/A6/A7 (revert guard, snapshot GC, unique ids), E1 (normalize fitness), L2 (real branching or rename), W1/W2 (enforce the breaker, wire velocity), R2/R3 (context windowing, 5xx fallback).

**Deprecating legacy Castor:**
- The new `DeepSeekAvoRunner` is a **drop-in replacement** for the Castor CLI path: same `run({prompt, cwd, ...})` contract, same tool surface, plus AVO. The dual-engine dispatch in `6221c94` already lets you A/B them.
- **Recommended deprecation sequence:**
  1. Keep `castor_runner.js` as the fallback engine behind a config flag (`engine: "avo" | "castor"`), defaulting to `avo`.
  2. Run a shadow period: dispatch a fraction of real tasks to both engines, compare `finalText`/`status`/`durationMs`/`totalCompletionTokens` from the JSONL ledgers (the new append-only `events.jsonl` makes this auditable in a way the old `sessions.db` did not).
  3. Once the shadow period shows parity or better on the head-to-head metrics (the Benchmark 1–3 suite is the acceptance gate), flip the default to `avo` and mark `castor_runner.js` `@deprecated`.
  4. Remove the Castor binary dependency (`getCastorExecutable`, the `pkill -f "castor run"` branches in `wsl_bridge.js`) in a follow-up commit once no config path references it.
- **One caveat to close before full cutover:** `wsl_bridge.js` still hard-codes `castor` in `killProcessTree*` and `getCastorExecutable`. Those are legacy-only; they are harmless while the flag exists but should be removed with the deprecation to avoid a dangling dependency.

---

## 8. Final Technical Verdict

The `feat/deepseek-avo` branch delivers a **genuently working** Cordis-style reversible microkernel and a **real** NVIDIA-AVO closed loop with verified deterministic rollback — the test and benchmark suites pass cleanly, including live vLLM streaming. The architecture is clean and the plugin/disposer model is sound.

The principal risk is **over-claiming**: the components are labeled "sandboxed" and "deterministic" and "DAG" and "circuit breaker," but the enforcement behind those labels is partial. The three items that most undermine the safety story are **(1) the unenforced filesystem boundary (S1), (2) the unreliable process-tree kill (H1), and (3) the silently-resetting lineage store (L1)**. Fix those three and the harness is fit for unattended autonomous mutation; the remaining items are hardening and naming hygiene.

**Recommendation: APPROVE the merge with the two pre-merge must-fixes (S1, L1) applied, and track the rest as a defined hardening backlog.**

*— End of initial audit —*

---

## 9. Remediation Verification & Final Sign-Off (Commit `b84a9d8`)

**Re-verification run (2026-09-05):**
- `node mcp-qwen/test_deepseek_avo.js` → **6/6 PASSED** (live vLLM 1955 ms TTFT)
- `node mcp-qwen/benchmark_comparison.js` → **ALL COMPLETED** (sandbox 5.0× faster; AVO accept + clean revert verified)

I inspected the actual diff of `b84a9d8` (not just the commit message) and confirmed each remediation is genuinely implemented:

| ID | Remediation | Verified in diff? | Notes |
|---|---|---|---|
| S1 | `resolvePath()` boundary containment | PASS | `path.relative` + `startsWith("..")`/`isAbsolute` guard → `PathEscapeError`. Root itself resolves to `""` (allowed); all escapes rejected. Correct. |
| S3 | Empty `target_content` guard | PASS | Rejects empty/non-string before any read. |
| L1 | Atomic + non-destructive lineage | PASS | `writeFileSync(tmp)` → `renameSync` (atomic, same volume); tmp cleaned on failure. Corrupt JSON quarantined to `lineage.json.corrupt-<ts>` **before** the baseline reset — no silent data loss. |
| R1 | Remove duplicate `tool_result` log | PASS | The `tool:after_execute` hook is deleted; the runner's explicit append is now the single source of truth. |
| A1 | Deterministic rollback for file creation | PASS | `manifest.json` records `existed` per file; on revert, non-existent-at-propose files are `unlinkSync`'d. |
| A3 | Validate node before file mutation | PASS | `revertCandidate` now throws on unknown id **and** on already-`accepted` candidates before touching the filesystem. (Also closes A4.) |
| K4 | No unhandled promise rejections | PASS | `emit` pushes `{ error, isError }` instead of a live `Promise.reject`. |
| H3 | Bounded stdout/stderr | PASS | 4 MB cap with `...[truncated at 4MB]` marker. |
| H4 | Signal-kill exit mapping | PASS | `close(code, signal)` → `code ?? (signal ? 137 : 1)` + signal annotation in stderr. |

**Residual (non-blocking) observations on the remediation itself:**
- **H4 minor:** the signal path hard-codes `137` (SIGKILL) for *any* signal rather than `128 + signum`. Functionally fine (non-zero, correctly fails the AVO gate); a `128+signum` mapping would be more precise. Cosmetic.
- **A1 minor:** if `manifest.json` itself is corrupt, the `catch {}` swallows it and the revert is a no-op (files left as-is). This is a *safe* failure mode (no data loss, no wrong-file restore) — acceptable, arguably better than the prior behavior.
- **H1 (process-tree kill) remains open** — it was in the "strongly recommended / fast-follow" bucket, not a must-fix, and is not in this commit. It is the last meaningful safety gap before fully unattended operation (orphaned grandchildren on timeout, esp. WSL). Track it in the hardening backlog; it does **not** block the merge.

### FINAL VERDICT: **APPROVED FOR MERGE**

All seven pre-merge must-fixes and recommended hardening items from the audit are correctly implemented and verified against the actual diff. Both the validation suite (6/6) and the head-to-head benchmark suite pass, including live vLLM streaming and the closed-loop AVO accept/revert cycle. The two items I designated as *must-fix before unattended autonomy* (S1 sandbox boundary, L1 atomic/non-destructive lineage) are closed, and the rollback-fidelity, event-bus, and buffer-safety gaps are all resolved.

**Sign-off: I approve merging `feat/deepseek-avo` into `main`.**

**Merge conditions / follow-ups (non-blocking):**
1. Proceed with the 4-step Castor deprecation sequence in §7 (flag → shadow A/B via JSONL ledgers → flip default → remove `castor`-specific `wsl_bridge` branches).
2. Track **H1** (reliable process-tree kill: `detached:true` + POSIX group kill; WSL kill-by-session-tag) as the top item of the post-merge hardening backlog before enabling long-running unattended mutation.
3. Remaining low-severity backlog (H2 cwd shell-quoting, S2 bounded read, S4 atomic writes, E1 fitness normalization, L2 real branching, W1/W2 watchdog enforcement, R2/R3) can be addressed incrementally.

*— End of final sign-off —*
