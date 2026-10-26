---
name: evo-mutation-rollback
description: Safe propose, evaluate, select, revert discipline for Evo mutations — snapshot the working tree, mutate one concern, gate on deterministic fitness, and commit or atomically roll back.
keywords: [evo, mutation, rollback, fitness, candidate, revert, optimize]
---
# Evo Mutation Rollback

A mutation is only safe if it can always be undone. Nothing is committed
until a deterministic fitness gate has passed, and a regression never
leaves the tree dirty.

## Sequence

1. **Baseline & Snapshot Verification** — Confirm the working tree is
   clean before mutating:
   - `git_status` → must report no pending changes (this is the
     rollback anchor; if the tree is dirty, stop and reconcile first).
   - `git_diff` → must be empty; record any intentionally pre-staged
     context.
   - Record the **baseline fitness score** (the current
     test/benchmark result for the metric under optimization) and the
     **exact list of target files** the mutation may touch.
2. **Surgical Single-Concern Mutation** — Apply the mutation strictly to
   the declared target files, using:
   - `ast_replace` — preferred for structural (syntactic) changes; it
     performs in-memory AST validation and rolls back automatically on
     a syntax error, so a failed mutation never dirties disk.
   - `edit_file` — for exact-substring edits; likewise AST-validated
     before the on-disk commit.
   Never edit a file that is not on the target list — changes outside
   the list are not covered by the rollback.
3. **Deterministic Fitness Gate** — Run the verification command via
   `bash`. Use exactly the gate appropriate to the workspace:
   - `cargo test --all-features`
   - `cargo clippy --all-targets --all-features -- -D warnings`
   - `castor evo run` (for the repo's own Evo task suites)
   Capture the fitness score and the compact failure digest (see the
   `traceback-condensing` skill — never paste raw multi-page output).
4. **Selection or Atomic Rollback** — Decide on the digest:
   - **Select**: fitness strictly improved (or held neutral on an
     explicit simplification pass) with **zero regressions**. Commit
     the candidate via `git_commit(paths: [...])` listing exactly the
     mutated files.
   - **Rollback**: fitness regressed, any test failed, or the digest
     shows a real bug. Immediately restore the touched files via
     `bash`: `git checkout -- <paths>`, then confirm a clean tree via
     `git_status` before continuing.

## Rules

- The snapshot verification (step 1) must always precede any file
  mutation; an interrupted mutation is rolled back, never resumed
  mid-edit.
- Never batch multiple concerns into one candidate — the fitness delta
  must be attributable to a single change.
- If a step is interrupted by a timeout or budget, the next action is
  to roll back to the last good state, not to resume mid-mutation.
