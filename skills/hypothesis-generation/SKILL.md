---
name: hypothesis-generation
description: Structured root-cause and optimization hypothesis generation — gather evidence, state ranked falsifiable single-variable hypotheses, and discriminate with the minimal measurement.
keywords: [hypothesis, root-cause, investigate, debug, benchmark, diagnose, failure]
---
# Hypothesis Generation

Every code change you make to fix a bug or improve a metric is a
*falsifiable hypothesis*, not a guess. A hypothesis you cannot measure
is not a hypothesis — do not propose it.

## Checklist

1. **Evidence Gathering** — Before writing a hypothesis, inspect:
   - Failure traces and build output (condense with the
     `traceback-condensing` skill first — never reason over raw
     multi-page logs).
   - Code structure around the failing frame: `ast_search` and
     `search_code` for the symbols and call sites; `read_file` for the
     exact region.
   - Recent history: `git_log` (what changed) and `git_diff` (how it
     changed) for the relevant paths.
2. **Ranked Falsifiable Hypotheses** — Formulate **1–3**
   single-variable hypotheses, in decreasing order of probability.
   Each hypothesis must state all three of:
   - **(a)** the **exact code coordinate** (`file:line`) where the
     mechanism lives;
   - **(b)** the **predicted mechanism** — one sentence: "Changing X
     will cause Y because <mechanism>";
   - **(c)** the **minimal reproduction or measurement command**
     (`python_repl` for arithmetic/data checks, `bash` for
     test/benchmark commands) that falsifies the hypothesis in a
     single pass.
3. **Single-Variable Discrimination** — Test the highest-probability
   hypothesis first. Each trial changes exactly one variable; never
   bundle multiple unverified changes into a single trial — you cannot
   attribute the delta. If the hypothesis is falsified, move to the
   next-ranked one; if confirmed, stop and apply the fix (with the
   `evo-mutation-rollback` discipline if it touches fitness metrics).

## Rules

- A hypothesis without an exact `file:line` coordinate is a rumour;
  read the file until you have the coordinate.
- Keep the hypothesis text short and specific — it is the unit of
  attribution in any subsequent lineage or audit record.
- The falsification command must be runnable in one pass and must
  produce a decisive yes/no, not an open-ended exploration.
