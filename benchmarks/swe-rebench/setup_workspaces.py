#!/usr/bin/env python3
"""Build and populate fair, identical benchmark workspaces across 3 arms:
1. solo_gemini
2. solo_qwen
3. castor

Ensures:
- Exact base_commit
- No pre-existing .venv (lack of .venv parity)
- Uniform prompt template with project-specific venv instruction
- 'Do not use Castor' instruction exclusively on solo_gemini
"""
import json
import shutil
import subprocess
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
TASKS_JSONL = HERE / "predictions" / "corpus_b" / "corpus_b_tasks.jsonl"
WORKSPACES_ROOT = HERE / "workspaces"

ARMS = ["solo_gemini", "solo_qwen", "castor"]

TASK_DEFS = [
    {
        "folder": "task_1_sqlglot_8575",
        "id": "tobymao__sqlglot-8575",
        "repo": "tobymao/sqlglot",
        "src_repo": REPO_ROOT / ".scratch" / "swe-pilot" / "tobymao__sqlglot-7187",
        "base_commit": "9f2f4cd5a2d25e967142d0c90d107128d4dc70e5",
        "test_cmd": "pytest tests/dialects/test_bigquery.py tests/dialects/test_duckdb.py",
    },
    {
        "folder": "task_2_sqlglot_8574",
        "id": "tobymao__sqlglot-8574",
        "repo": "tobymao/sqlglot",
        "src_repo": REPO_ROOT / ".scratch" / "swe-pilot" / "tobymao__sqlglot-7187",
        "base_commit": "1e2cc210e2a336defd9037dfc66d9dae1e72780e",
        "test_cmd": "pytest tests/dialects/test_oracle.py tests/dialects/test_postgres.py",
    },
    {
        "folder": "task_3_sqlglot_8573",
        "id": "tobymao__sqlglot-8573",
        "repo": "tobymao/sqlglot",
        "src_repo": REPO_ROOT / ".scratch" / "swe-pilot" / "tobymao__sqlglot-7187",
        "base_commit": "ffe2c73694fc88e5b44b0deff1f1fb92a6a9fe95",
        "test_cmd": "pytest tests/fixtures/optimizer/simplify.sql tests/fixtures/optimizer/optimizer.sql",
    },
    {
        "folder": "task_4_nicegui_6380",
        "id": "zauberzeug__nicegui-6380",
        "repo": "zauberzeug/nicegui",
        "src_repo": REPO_ROOT / ".scratch" / "swe-pilot" / "zauberzeug__nicegui-5858",
        "base_commit": "df3dcc3949bd2d0642e044895aefd4bd10c38349",
        "test_cmd": "pytest tests/test_scene.py",
    },
    {
        "folder": "task_5_nicegui_6364",
        "id": "zauberzeug__nicegui-6364",
        "repo": "zauberzeug/nicegui",
        "src_repo": REPO_ROOT / ".scratch" / "swe-pilot" / "zauberzeug__nicegui-5858",
        "base_commit": "e8689eb3dde15979b7dc4ace955d7b1c50999893",
        "test_cmd": "pytest tests/test_storage.py",
    },
    {
        "folder": "task_6_click_3884",
        "id": "pallets__click-3884",
        "repo": "pallets/click",
        "src_repo": REPO_ROOT / ".scratch" / "cache" / "pallets__click",
        "base_commit": "bb695aaecdef95832d167fc1d508921745aa824e",
        "test_cmd": "pytest tests/test_options.py",
    },
]


def main():
    # 1. Load problem statements
    problems_by_id = {}
    with open(TASKS_JSONL, "r", encoding="utf-8") as f:
        for line in f:
            if line.strip():
                row = json.loads(line)
                problems_by_id[row["instance_id"]] = row

    # 2. Wipe old scratch corpus_b if present
    scratch_corpus_b = REPO_ROOT / ".scratch" / "corpus_b"
    if scratch_corpus_b.exists():
        print(f"Wiping legacy {scratch_corpus_b}...")
        shutil.rmtree(scratch_corpus_b, ignore_errors=True)

    WORKSPACES_ROOT.mkdir(parents=True, exist_ok=True)

    # 3. Create 3 identical arms
    for arm in ARMS:
        arm_dir = WORKSPACES_ROOT / arm
        arm_dir.mkdir(parents=True, exist_ok=True)
        print(f"\n================ Preparing Arm: {arm} ================")

        for t in TASK_DEFS:
            target_dir = arm_dir / t["folder"]
            if target_dir.exists():
                print(f"Resetting existing {target_dir}...")
                shutil.rmtree(target_dir, ignore_errors=True)

            print(f"Cloning {t['id']} -> {target_dir}...")
            subprocess.check_call(
                ["git", "clone", str(t["src_repo"].resolve()), str(target_dir)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            subprocess.check_call(
                ["git", "-C", str(target_dir), "checkout", t["base_commit"]],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )

            # Ensure clean state: remove any .venv or python virtualenvs
            for venv_name in [".venv", "venv", "env"]:
                v = target_dir / venv_name
                if v.exists():
                    shutil.rmtree(v, ignore_errors=True)

            # Construct PROMPT.md
            task_info = problems_by_id[t["id"]]
            arm_instructions = [
                "- Solve the issue described below. Inspect the repository, locate the relevant files, reproduce the bug if necessary, implement the fix, and ensure all existing and new tests pass.",
                "- Use project-specific venv (pip/uv) only; do not rely on global environment.",
            ]
            if arm == "solo_gemini":
                arm_instructions.append("- Do not use Castor.")

            instructions_text = "\n".join(arm_instructions)
            prompt_content = f"""# Benchmark Task: {t['id']} ({arm})

**Repository**: `{t['repo']}`  
**Base Commit**: `{t['base_commit']}`  
**Merged Date**: `{task_info['merged_at']}` (October 2026, Zero-Contamination)

---

## Instructions
{instructions_text}

---

## Problem Statement

{task_info['problem_statement']}
"""
            (target_dir / "PROMPT.md").write_text(prompt_content, encoding="utf-8")

            # Write TASK_META.json
            meta = {
                "instance_id": t["id"],
                "repo": t["repo"],
                "base_commit": t["base_commit"],
                "test_cmd": t["test_cmd"],
                "merged_at": task_info["merged_at"],
                "arm": arm,
            }
            (target_dir / "TASK_META.json").write_text(json.dumps(meta, indent=2), encoding="utf-8")
            print(f"  [OK] {t['folder']} ready (HEAD: {t['base_commit'][:10]})")

    print("\n[SUCCESS] All 18 benchmark workspaces ready in benchmarks/swe-rebench/workspaces/")


if __name__ == "__main__":
    main()
