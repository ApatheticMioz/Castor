#!/usr/bin/env python3
"""Record and grade an evaluation run on Corpus B across the 3 arms.

Usage:
    python benchmarks/swe-rebench/record_run.py --arm solo_gemini --task task_1_sqlglot_8575 [--conv <conversation_id>]
    python benchmarks/swe-rebench/record_run.py --arm solo_qwen   --task task_1_sqlglot_8575 [--conv <conversation_id>]
    python benchmarks/swe-rebench/record_run.py --arm castor      --task task_1_sqlglot_8575 [--conv <conversation_id>]
"""
import argparse
import datetime
import json
import os
import subprocess
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
TASKS_JSONL = HERE / "predictions" / "corpus_b" / "corpus_b_tasks.jsonl"
WORKSPACES_ROOT = HERE / "workspaces"
APP_DATA = Path(os.environ.get("USERPROFILE", "")) / ".gemini" / "antigravity-ide" / "brain"


def get_latest_conversation_id():
    if not APP_DATA.exists():
        return None
    conv_dirs = [d for d in APP_DATA.iterdir() if d.is_dir() and not d.name.startswith(".")]
    if not conv_dirs:
        return None
    conv_dirs.sort(key=lambda d: d.stat().st_mtime, reverse=True)
    return conv_dirs[0].name


def parse_transcript(conv_id):
    transcript_file = APP_DATA / conv_id / ".system_generated" / "logs" / "transcript.jsonl"
    if not transcript_file.exists():
        return {}

    turns = 0
    tool_calls = 0
    start_time = None
    end_time = None

    with open(transcript_file, "r", encoding="utf-8") as f:
        for line in f:
            if not line.strip():
                continue
            entry = json.loads(line)
            ts_str = entry.get("created_at")
            if ts_str:
                try:
                    ts = datetime.datetime.fromisoformat(ts_str.replace("Z", "+00:00"))
                    if start_time is None or ts < start_time:
                        start_time = ts
                    if end_time is None or ts > end_time:
                        end_time = ts
                except Exception:
                    pass

            if entry.get("type") in ("PLANNER_RESPONSE", "USER_INPUT"):
                turns += 1
            if entry.get("tool_calls"):
                tool_calls += len(entry["tool_calls"])

    duration_s = (end_time - start_time).total_seconds() if (start_time and end_time) else 0.0
    return {
        "conversation_id": conv_id,
        "duration_s": round(duration_s, 1),
        "turns": turns,
        "tool_calls": tool_calls,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm", required=True, choices=["solo_gemini", "solo_qwen", "castor"],
                    help="Benchmark arm: solo_gemini | solo_qwen | castor")
    ap.add_argument("--task", required=True, help="Task folder name, e.g. task_1_sqlglot_8575")
    ap.add_argument("--conv", default=None, help="Antigravity conversation ID (defaults to most recent)")
    args = ap.parse_args()

    task_dir = WORKSPACES_ROOT / args.arm / args.task
    if not task_dir.exists():
        raise SystemExit(f"Task dir {task_dir} does not exist")

    meta_file = task_dir / "TASK_META.json"
    if not meta_file.exists():
        raise SystemExit(f"TASK_META.json not found in {task_dir}")
    meta = json.loads(meta_file.read_text(encoding="utf-8"))
    instance_id = meta["instance_id"]

    # 1. Capture git diff from task dir
    patch = subprocess.check_output(["git", "-C", str(task_dir), "diff"]).decode("utf-8")
    if not patch.strip():
        print(f"WARNING: git diff in {task_dir} is completely empty!")

    # 2. Extract conversation metrics
    conv_id = args.conv or get_latest_conversation_id()
    metrics = parse_transcript(conv_id) if conv_id else {}

    # 3. Append to arm predictions file
    out_file = HERE / "predictions" / "corpus_b" / f"{args.arm}.jsonl"
    out_file.parent.mkdir(parents=True, exist_ok=True)

    record = {
        "instance_id": instance_id,
        "model_name_or_path": args.arm,
        "model_patch": patch,
        "metrics": metrics,
    }

    # Resumable / update existing entry
    existing = []
    if out_file.exists():
        for line in out_file.read_text(encoding="utf-8").splitlines():
            if line.strip():
                row = json.loads(line)
                if row["instance_id"] != instance_id:
                    existing.append(row)
    existing.append(record)

    with open(out_file, "w", encoding="utf-8") as f:
        for row in existing:
            f.write(json.dumps(row, ensure_ascii=False) + "\n")

    print(f"Recorded prediction for {instance_id} to {out_file}")
    print(f"Patch size: {len(patch)} bytes | Duration: {metrics.get('duration_s', 'N/A')}s | Tool calls: {metrics.get('tool_calls', 'N/A')}")

    # 4. Grade against test_patch
    tasks_lookup = {}
    with open(TASKS_JSONL, "r", encoding="utf-8") as f:
        for l in f:
            r = json.loads(l)
            tasks_lookup[r["instance_id"]] = r

    test_patch = tasks_lookup.get(instance_id, {}).get("test_patch", "")
    if test_patch:
        print("\n--- Running Grading Verification ---")
        patch_file = task_dir / ".test_patch.diff"
        with open(patch_file, "wb") as f:
            f.write(test_patch.encode("utf-8"))
        try:
            # Apply test patch (binary LF)
            subprocess.check_call(["git", "-C", str(task_dir), "apply", ".test_patch.diff"])
            # Run test command (try Windows first, fall back to WSL)
            test_cmd = meta.get("test_cmd", "pytest")
            res = subprocess.run(test_cmd, cwd=str(task_dir), shell=True, capture_output=True, text=True)
            if res.returncode != 0 and "not recognized" in (res.stderr + res.stdout):
                # Run inside WSL
                drive_letter = task_dir.drive.lower().replace(":", "")
                posix_subpath = task_dir.as_posix()[3:]
                wsl_cmd = f"wsl bash -c \"cd /mnt/{drive_letter}/{posix_subpath} && ~/.local/bin/pytest {test_cmd.replace('pytest', '')}\""
                res = subprocess.run(wsl_cmd, shell=True, capture_output=True, text=True)
            if res.returncode == 0:
                print(f"RESULT: 100% PASS (All unit tests passed cleanly)")
            else:
                print(f"RESULT: FAILED (Exit code {res.returncode})")
                print("Stderr tail:\n", res.stderr[-1000:])
        except Exception as e:
            print(f"Grading error: {e}")
        finally:
            # Revert test patch so workspace stays at model's state
            subprocess.call(["git", "-C", str(task_dir), "apply", "-R", ".test_patch.diff"])
            if patch_file.exists():
                patch_file.unlink()


if __name__ == "__main__":
    main()
