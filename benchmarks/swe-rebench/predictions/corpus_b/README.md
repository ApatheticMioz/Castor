# Corpus B: Zero-Contamination SWE Benchmark (October 2026 Split)

## 1. Overview & Verification

This dataset was freshly harvested from GitHub on **October 10, 2026**. All tasks are derived from pull requests merged between **October 3 and October 9, 2026**.

* **Data Contamination**: **0.00%** for both Gemini 3.8 Flash and Qwen3.8-27B (all tasks post-date the pretraining and instruction-tuning cutoffs of both models).
* **Ground Truth**: Each task has an explicit problem description, exact base commit hash, held-out unit test diff (`test_patch`), and reference fix (`gold_patch`).

Task file: [`corpus_b_tasks.jsonl`](file:///d:/LLM_Ecosystem/benchmarks/swe-rebench/predictions/corpus_b/corpus_b_tasks.jsonl)

---

## 2. Harvested Task Cards

### Task 1: `tobymao__sqlglot-8575`
* **Repo**: `tobymao/sqlglot`
* **Base Commit**: `9f2f4cd5a2d25e967142d0c90d107128d4dc70e5`
* **Merged**: 2026-10-09
* **Problem**: Fix incorrect parse of a `SELECT [a, b, ...]` in BigQuery and DuckDB dialects when the first element in the array is a subquery.
  ```python
  >>> sqlglot.transpile("SELECT [(SELECT 1), 2]", read="bigquery", write="bigquery")
  # Buggy output: ['SELECT ARRAY((SELECT 1))']  # Drops element 2!
  ```
* **Target Files**: `sqlglot/dialects/dialect.py`
* **Test Verification**: `pytest tests/dialects/test_bigquery.py tests/dialects/test_duckdb.py`

### Task 2: `tobymao__sqlglot-8574`
* **Repo**: `tobymao/sqlglot`
* **Base Commit**: `1e2cc210e2a336defd9037dfc66d9dae1e72780e`
* **Merged**: 2026-10-09
* **Problem**: In a recursive CTE, `SEARCH` followed by `CYCLE` fails to parse, and `CYCLE` with `TO ... DEFAULT` fails with `ParseError`.
  ```python
  cte = "WITH t(id, mgr) AS (SELECT 1, NULL FROM dual UNION ALL SELECT e.id, e.mgr FROM t r, emp e WHERE r.id = e.mgr)"
  sqlglot.parse_one(f"{cte} SEARCH DEPTH FIRST BY id SET ord CYCLE id SET is_cycle TO 'Y' DEFAULT 'N' SELECT * FROM t", read="oracle")
  # ParseError: alias does not support CTE
  ```
* **Target Files**: `sqlglot/expressions/query.py`, `sqlglot/generator.py`, `sqlglot/parser.py`
* **Test Verification**: `pytest tests/dialects/test_oracle.py tests/dialects/test_postgres.py`

### Task 3: `tobymao__sqlglot-8573`
* **Repo**: `tobymao/sqlglot`
* **Base Commit**: `ffe2c73694fc88e5b44b0deff1f1fb92a6a9fe95`
* **Merged**: 2026-10-09
* **Problem**: `optimize("select CASE WHEN TRUE THEN (SELECT 1) ELSE (SELECT 2) END")` raises `TypeError: sqlglot.expressions.core.Condition object expected; got sqlglot.expressions.query.Subquery`.
* **Target Files**: `sqlglot/optimizer/simplify.py`
* **Test Verification**: `pytest tests/fixtures/optimizer/simplify.sql tests/fixtures/optimizer/optimizer.sql`

### Task 4: `zauberzeug__nicegui-6380`
* **Repo**: `zauberzeug/nicegui`
* **Base Commit**: `df3dcc3949bd2d0642e044895aefd4bd10c38349`
* **Merged**: 2026-10-07
* **Problem**: `ui.scene` objects created before the scene component is mounted (e.g. inside an inactive `ui.tab_panel`) never appear because client drops the create call and first `init` does not resend existing objects.
* **Target Files**: `nicegui/elements/scene/scene.py`, `nicegui/elements/scene/scene_object3d.py`
* **Test Verification**: `pytest tests/test_scene.py`

### Task 5: `zauberzeug__nicegui-6364`
* **Repo**: `zauberzeug/nicegui`
* **Base Commit**: `e8689eb3dde15979b7dc4ace955d7b1c50999893`
* **Merged**: 2026-10-04
* **Problem**: Duplicated browser tab shares nested collections in `app.storage.tab` with the original tab. Mutating a list in the duplicate mutates the original tab.
* **Target Files**: `nicegui/storage.py`
* **Test Verification**: `pytest tests/test_storage.py`

### Task 6: `pallets__click-3884`
* **Repo**: `pallets/click`
* **Base Commit**: `bb695aaecdef95832d167fc1d508921745aa824e`
* **Merged**: 2026-10-06
* **Problem**: Fix `show_envvar` output format for empty environment variable values (`envvar=""` -> `(env var: '')` should be suppressed) and sequences (`envvar=['A', 'B']` -> `(env var: '['A', 'B']')` should render as comma-separated `(env var: 'A', 'B')`).
* **Target Files**: `src/click/core.py`
* **Test Verification**: `pytest tests/test_options.py`

---

## 3. How to Run the 3-Arm Matched-Control Benchmark

### Arm 1: Solo Gemini (Cloud Baseline)
1. In a fresh chat session, clone the target repo and checkout `base_commit`.
2. Provide the raw `problem_statement` text.
3. Allow Gemini to use standard cloud file and bash tools to inspect, edit, and produce a patch.
4. Record: Wall-clock time, total cloud tokens, and whether test pass.

### Arm 2: Solo Qwen (Local Baseline @ $0)
1. Run local Qwen3.8-27B against the raw `problem_statement` using standard tool prompt (no lead architect steering).
2. Record: Wall-clock time, turn count, and whether test pass.

### Arm 3: Gemini + Castor (Pair-Programming Treatment)
1. Gemini operates as Lead Architect, decomposing the issue and steering Castor via `castor_coworker`.
2. Castor runs structural AST search/replace (`ast_replace`), syntax gate, and test execution locally at $0 token cost.
3. Truncation protection and disk spillover prevent context blowouts.
4. Record: Cloud tokens (drastically reduced), pass rate, and patch quality.
