# Castor Evals — Spec & Doctrine

Source of truth for the eval track. Doctrine from `evals-faq.pdf` (Husain &
Shankar): **error analysis first, deterministic-only CI gate, binary pass/fail,
no imagined tasks.** Every task below was derived from a *real* failure class
observed in `~/.castor/sessions/*/events.jsonl` (see §Failure Classes).

## 1. Failure Classes

Classification of observed error classes across session event ledgers:

| # | Class | Count | Evidence (event type) |
|---|-------|-------|------------------------|
| F1 | Tool errors (env/config) | 14 | `tool_result.error`: MissingBraveApiKey×6, MissingTavilyApiKey×4, MissingSearxngUrl×3, DDG rate-limit×1 |
| F2 | Tool errors (bad target) | 7 | web_fetch 429/404×5, list_dir "Directory not found"×1, edit_file "Target content not found"×1 |
| F3 | Budget exhaustion | 23 | prompt_over_budget×11, probe_budget_warning×8, context_depth_warning×4 |
| F4 | Degenerate final / forced continuation | 5 | empty_stream_retry(reason=degenerate_final)×1, continuation_injected(reason=reasoning_ceiling)×4 |
| F5 | Action loop (repeated identical call) | 1 | action_loop_detected (bash, repeats=3) |
| F6 | Dropped tool-call | 1 | tool_call events (384) > tool_result events (383) in same session |

Plus a chronic context-bloat signal: `tool_output_spilled`×61 (outputs >40 KB
spilled to `.scratch/`), which drives F3.

## 2. Task format — `tasks/<id>/task.toml`

Parsed by `src/evals/fixture.rs` (`TaskConfig`, `deny_unknown_fields`), so the
field set below is exact and any unknown key is a load error:

```toml
id = "fix-failing-test"          # unique, kebab-case; == directory name
prompt = """..."""               # exact user prompt given to the agent
setup = "setup.sh"               # optional; prepares fixture (deterministic, offline)
timeout_s = 120                  # hard wall-clock cap for Tier B runs
tags = ["tier-a", "F2"]          # tier-a|tier-b + failure class(es) it guards

# Deterministic scorer: an ORDERED array of check tables.
[[scorer.checks]]
id = "tests-green"
kind = "exec"                    # run `cmd` in the post-run workdir
cmd = "node --test \"test/*.test.js\""
expect_exit = 0

[[scorer.checks]]
id = "test-file-untouched"
kind = "file"                    # assert on a workdir file
path = "test/sum.test.js"
expect_sha256 = "930ebcff…"      # or: expect_contains = "…"

[[scorer.checks]]
id = "no-dropped-toolcall"
kind = "trace"                   # predicate over trace.jsonl
assert = "every_tool_call_has_result"
```

Check `kind`s are **deterministic only**: (a) `exec` — run `cmd` in the post-run
workdir and assert `expect_exit`; (b) `file` — assert on a workdir file via
`expect_sha256` or `expect_contains`; (c) `trace` — a named predicate over
`trace.jsonl` (`every_tool_call_has_result`, `final_message_nonempty`,
`no_tool_output_spilled`, `no_write_outside_workdir`,
`final_message_mentions_workspace`, `read_file_before_edit_file`,
`edit_file_before_bash_verify`, `bash_verify_present`,
`edit_file_count_eq_1`, `no_consecutive_identical_failed_toolcall`).
No LLM judges, no similarity metrics, no Likert scores.

## 3. Trace fixture format — `trace.jsonl`

One JSON object per line, mirroring the real Castor session event log. A
fixture trace records **every** LLM response and every tool call/result:

```jsonl
{"timestamp":"...","sessionId":"t","type":"session_start","harness":"Castor","version":"1.0.0","cwd":"/work","prompt":"..."}
{"timestamp":"...","type":"assistant_message","content":"...","toolCalls":[{"id":"chatcmpl-tool-1","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"src/a.js\"}"}}]}
{"timestamp":"...","type":"tool_call","toolCallId":"chatcmpl-tool-1","name":"read_file","args":{"path":"src/a.js"}}
{"timestamp":"...","type":"tool_result","toolCallId":"chatcmpl-tool-1","toolName":"read_file","result":{...},"isError":false,"latencyMs":3}
{"timestamp":"...","type":"session_end","status":"completed","turnsTaken":3,"continuationsInjected":0,"durationMs":4200,"totalCompletionTokens":900}
```

Invariants a scorer may rely on: every `tool_call` has a matching
`tool_result` (a missing one is itself a failure, class F6); `assistant_message`
carries the raw model response; `session_end` is the last line.

## 4. Anti-green rules (non-negotiable)

1. **One hard canary per suite, expected to FAIL.** `canary-hard` is a task the
   current agent is known to fail. The gate is only trusted if it stays red.
   A canary that starts passing without a code change means the scorer is
   broken — investigate the scorer, not the agent.
2. **Known-failing variant per task.** Each task ships a `known-fail/` variant
   (a recorded trace of a real bad run) that the scorer must mark FAIL. If the
   scorer passes it, the scorer is too weak.
3. **ERROR ≠ FAIL.** A check that throws (scorer bug, missing fixture, harness
   crash) is `ERROR` (exit 2), never `FAIL` (exit 1). The gate reports ERROR
   loudly and blocks; it is never silently converted to pass or fail.
4. **Never tune a scorer to make a task pass.** Scorer changes require a
   failing known-fail variant to still fail and the canary to still fail.
   Weakening a check to unblock the build hides the regression.
5. **No LLM judges in the gate.** CI checks are execution + trace assertions
   only. LLM-as-judge is allowed in Tier B nightly *monitoring* (async,
   reference-free) but never in the blocking gate.

## 5. Tiers

- **Tier A — offline replay (CI, every commit).** No LLM. The "agent" is a
  recorded `trace.jsonl` fixture; the runner materializes the fixture repo,
  applies the recorded file mutations, runs the deterministic scorer.
  Deterministic, seconds, offline. Catches scorer regressions and guards
  known failure classes F1–F6.
- **Tier B — live nightly.** Real agent run against the fixture repo; the
  harness records `events.jsonl` as the trace; same scorer applies.
  Non-deterministic by nature → results feed monitoring dashboards and
  error analysis, not the blocking gate. New failure patterns found here are
  promoted to Tier A fixtures (the CI/production feedback loop).

## 6. Layout

```
castor/evals/
  EVALS.md
  tasks/<id>/
    task.toml
    setup.sh            # optional, offline
    fixture/            # KB-scale repo the agent works in
    trace.jsonl         # Tier A golden trace (expected-pass)
    known-fail/trace.jsonl   # must score FAIL
  tasks/canary-hard/    # expected-FAIL canary
```
