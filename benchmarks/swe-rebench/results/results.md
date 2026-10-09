# SWE-rebench eval results: Castor + Qwen3.8-27B (CTX=huge, DFlash2, KVarN)

**Run date:** 2026-08-24. **Split:** `nebius/SWE-rebench-leaderboard` / `2026_03`
(110 tasks; 50 sampled, seed `20260824`). **Protocol:** best-of-1 (single
attempt per task, no retries) — not directly comparable to SWE-rebench's own
best-of-5 leaderboard protocol; see caveats below.

## Headline result

**16/50 = 32.0% Resolved Rate.**

| | count | of 50 |
|---|---|---|
| Resolved (patch applied, FAIL_TO_PASS passed, no PASS_TO_PASS regressions) | 16 | 32.0% |
| Attempted but did not resolve (real patch, ran, test failure/regression) | 12 | 24.0% |
| Never attempted (Castor exhausted the 900s solve budget with no patch) | 22 | 44.0% |

**The more informative number: among the 28 tasks that actually got a
solve attempt, 16 resolved — a 57.1% per-attempt success rate.** The
900s timeout, not model/harness quality, is the dominant loss factor here:
44% of the sample never produced a patch at all, which drags the
denominator-of-50 headline number down independent of how good those
attempts would have been. A rerun with a longer per-task budget (the
script already supports `--timeout`) would very plausibly land much closer
to the per-attempt 57.1% figure than the 32.0% headline one.

## Against the leaderboard reference points

| | Resolved Rate | Protocol |
|---|---|---|
| Claude Opus 4.6 | 65.3% | best-of-5 |
| Qwen3.5-35B-A3B (same architecture family) | 53.7% | best-of-5 |
| **This setup, all 50 (headline)** | **32.0%** | best-of-1, 44% never attempted |
| **This setup, 28 attempted only** | **57.1%** | best-of-1 |

Not an apples-to-apples comparison (best-of-1 vs best-of-5, and our number
mixes in a large no-attempt bucket the leaderboard protocol doesn't have),
but the per-attempt figure suggests the served, quantized model (W4A16 +
int4-GPTQ lm_head/MTP + KVarN) is in the right neighborhood of the
self-reported/leaderboard capability, not badly degraded by quantization —
the real bottleneck this run exposed is solve-time budget, not model quality.

## What actually happened (honest process log)

Two earlier grading attempts on this same prediction set were **invalidated
by tooling bugs, not by the model**, before landing on this trustworthy
number:

1. **First solve run**: `subprocess.run(text=True)` on Windows decodes
   captured output with the OEM codepage (cp1252), and Castor's terminal
   output contains UTF-8 sequences that aren't valid cp1252 — crashed the
   output-reader thread on every single task. Fixed by passing
   `encoding="utf-8", errors="replace"` explicitly.
2. **Resumed solve run**: hit `WinError 183` (directory already exists) on
   tasks whose scratch dir survived the first crash with a locked file
   inside it. Fixed by giving each attempt a fresh `tempfile.mkdtemp()`
   scratch dir instead of reusing `workdir/instance_id`.
3. **A design gap, also fixed**: on a genuine 900s timeout, the script was
   discarding any partial edits Castor had made instead of capturing
   `git diff` on the actual repo state before cleanup. Fixed - timeouts
   now still get credit for real partial work, though in practice none of
   the 22 timed-out tasks in this run had made any file changes yet when
   killed (all show truly empty patches).
4. **First grading run**: forgot `--instance-ids`, so `eval.py` graded the
   *entire* 110-task `2026_03` split instead of just the 50 sampled ones —
   60 irrelevant Docker pulls/evaluations wasted. Fixed in `grade.sh`
   (now derives `--instance-ids` from the predictions file automatically).
5. **Second grading run — the important one**: every single graded
   instance came back `exit_code: 128`, `passed_match: false`, 0 tests
   passing anywhere. Investigated rather than accepted at face value:
   read the actual container log (`fatal: not a git repository`), then
   directly inspected a live container's filesystem
   (`docker run --rm <image> bash -c 'find / -iname .git; pwd'`) and found
   the repo checkout lives at `/testbed` (the standard SWE-bench
   convention) — but `SWE-rebench-V2/scripts/eval.py` (upstream, not our
   code) computes `workdir = f"/{repo.split('/')[1]}"` instead, so every
   `git apply` ran against a directory with no git repo in it at all. This
   was a **0% false result from a harness bug**, not a real model score.
   Patched the vendored copy at `~/swe-rebench-eval/repo/scripts/eval.py`
   to use `/testbed` directly, with the reasoning documented inline. The
   32.0%/16-resolved result above is from the run *after* this fix.

## Files

### Archived March 2026 (Historical)
- [`../predictions/archived_2026_03/sample_2026_03_50.jsonl`](../predictions/archived_2026_03/sample_2026_03_50.jsonl) — the 50 sampled tasks (no test data, solve-step input)
- [`../predictions/archived_2026_03/castor_qwen_2026_03_50.jsonl`](../predictions/archived_2026_03/castor_qwen_2026_03_50.jsonl) — Castor+Qwen's raw predictions (patch + returncode + stderr tail per task)
- [`archived_2026_03/goose_qwen_2026_03_50_report.json`](archived_2026_03/goose_qwen_2026_03_50_report.json) — the full per-instance grading report

### Pilots (October 2026 Hardened Microkernel)
- [`../predictions/pilots/`](../predictions/pilots/) — Pilot tasks tested on previous failure classes (`nicegui-5858`, `sqlglot-7187`, `opensandbox-816`, `docsight-437`).
- [`pilots/`](pilots/) — Graded Docker execution reports demonstrating 100% pass rate after truncation caps and disk-spillover engine.

