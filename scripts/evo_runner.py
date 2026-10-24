#!/usr/bin/env python3
"""
Evo Autonomous Loop Runner (August 2026 SOTA Specification)

Standalone driver for the Evo evolution loop that the MCP server executes.
One round per invocation (re-run after each dispatch):

  1. Loads the candidate lineage from <dir>/.evo/lineage.json - the file
     written by EvoLineageEngine (mcp-castor/src/evo_engine.js), using that
     engine's real schema (candidates[] with candidateId/hypothesis/
     status/metricScore, plus bestCommit/bestMetric).
  2. Asks the local model (direct OpenAI-compatible call to :18020) for the
     next concrete, testable variation hypothesis given the objective and
     the recent lineage brief.
  3. Writes the ready-to-dispatch packet to <dir>/.evo/avq/ and prints the
     exact castor_coworker(...) MCP call (hypothesis / test_command /
     metric_name / higher_is_better all set, so the MCP server runs the
     verification test after the coworker task and records the outcome in the
     lineage itself).

This script never writes to .evo/lineage.json - the engine owns that file;
the runner only reads it and proposes. Standard library only (runs from any
Python 3, no venv needed).

Usage:
  python scripts/evo_runner.py --dir D:\\work --objective "raise bench throughput" \
      --test-cmd "pytest tests/ -q" --metric throughput
  python scripts/evo_runner.py ... --lower-is-better   # minimize the metric
  python scripts/evo_runner.py ... --dry-run           # skip the model call
"""

import argparse
import json
import os
import re
import time
import urllib.request

BASE_URL = "http://localhost:18020/v1"
MODEL = "qwen3.8-27b"


def get_api_key():
    key_path = os.path.expanduser("~/qwen-serving/api_key.txt")
    if os.path.exists(key_path):
        with open(key_path, "r", encoding="utf-8") as f:
            return f.read().strip()
    return os.environ.get("OPENAI_API_KEY", "")


def load_lineage(target_dir):
    path = os.path.join(target_dir, ".evo", "lineage.json")
    empty = {
        "bestCommit": None,
        "bestMetric": None,
        "targetMetricName": None,
        "higherIsBetter": True,
        "candidates": [],
    }
    if not os.path.exists(path):
        return empty
    try:
        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
        # Fill missing default keys if incomplete.
        for k, v in empty.items():
            data.setdefault(k, v)
        return data
    except Exception as e:
        print(f"Warning: could not parse {path} ({e}); using a fresh view.")
        return empty


def lineage_brief(lineage, n=5):
    candidates = lineage.get("candidates") or []
    best = lineage.get("bestMetric")
    if not candidates:
        return (
            f"Active Baseline Commit: {lineage.get('bestCommit') or 'unknown'} "
            f"(Best Metric: {best})\nNo previous attempts yet."
        )
    recent = candidates[-n:]
    lines = [
        '- [%s] Hypothesis: "%s" -> %s (Metric: %s)'
        % (
            c.get("candidateId"),
            c.get("hypothesis"),
            c.get("status"),
            c.get("metricScore"),
        )
        for c in recent
    ]
    return (
        f"Active Baseline Commit: {lineage.get('bestCommit')} "
        f"(Best Metric: {best})\nRecent attempts (last {len(recent)}):\n"
        + "\n".join(lines)
    )


def ask_model(objective, brief, test_cmd, metric, higher_is_better):
    direction = "maximize" if higher_is_better else "minimize"
    body = json.dumps(
        {
            "model": MODEL,
            "temperature": 0.7,
            "messages": [
                {
                    "role": "system",
                    "content": (
                        "You are the hypothesis generator for an Evo-style "
                        "evolutionary optimization loop. Propose exactly ONE concrete, "
                        "testable code-level variation that could improve the target "
                        "metric. Be specific about which file or behavior to change and "
                        "why it should help. Describe the mutation; do not write code."
                    ),
                },
                {
                    "role": "user",
                    "content": (
                        f"Objective: {objective}\n\n"
                        f"Verification test command (run automatically after each "
                        f"candidate is implemented): {test_cmd}\n"
                        f"Target metric (direction: {direction}): "
                        f"{metric or 'any measurable improvement'}\n\n"
                        f"Lineage so far:\n{brief}\n\n"
                        "Respond with the hypothesis in 2-4 sentences, then a one-line "
                        "'Mutation plan:' naming the concrete edit to make."
                    ),
                },
            ],
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        BASE_URL + "/chat/completions",
        data=body,
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {get_api_key()}",
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=300) as resp:
        return json.loads(resp.read().decode("utf-8"))["choices"][0]["message"]["content"]


def main():
    parser = argparse.ArgumentParser(
        description="Evo autonomous loop runner (one round per invocation)"
    )
    parser.add_argument("--dir", default=".", help="Target workspace directory")
    parser.add_argument("--objective", required=True, help="Overall research/optimization objective")
    parser.add_argument("--test-cmd", required=True, help="Verification command (e.g. 'pytest tests/')")
    parser.add_argument("--metric", default=None, help="Target metric name in test output")
    parser.add_argument(
        "--lower-is-better",
        action="store_true",
        help="Minimize the metric instead of maximizing it",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the dispatch packet without calling the model",
    )
    args = parser.parse_args()

    target_dir = os.path.abspath(args.dir)
    higher_is_better = not args.lower_is_better
    lineage = load_lineage(target_dir)

    print("=== Evo Autonomous Exploration Loop (one round) ===")
    print(f"Directory: {target_dir}")
    print(f"Objective: {args.objective}")
    print(f"Test Command: {args.test_cmd}")
    print(f"Metric: {args.metric or 'n/a'} ({'minimize' if args.lower_is_better else 'maximize'})\n")
    print(lineage_brief(lineage))
    print()

    if args.dry_run:
        hypothesis = "[dry-run] No model call made - fill in the hypothesis before dispatching."
    else:
        print("Generating candidate hypothesis from local model...")
        hypothesis = ask_model(
            args.objective, lineage_brief(lineage), args.test_cmd, args.metric, higher_is_better
        )
        print(f"\n--- Hypothesis ---\n{hypothesis}\n")

    # Persistent Evo session id: stable across rounds so the MCP server can
    # resume the same named session and vLLM prefix caching stays effective.
    slug = re.sub(r"[^a-z0-9]+", "_", args.objective.lower()).strip("_")[:24] or "evo"
    session_id = f"evo_{slug}"

    seq = len(lineage.get("candidates") or []) + 1
    avq_dir = os.path.join(target_dir, ".evo", "avq")
    os.makedirs(avq_dir, exist_ok=True)
    packet_path = os.path.join(avq_dir, f"candidate_{seq}_{int(time.time())}.md")

    instruction = (
        f"Implement the following Evo candidate variation in this workspace: {hypothesis}\n"
        "Make the smallest change that tests the hypothesis. Do NOT run the "
        "verification test yourself (it runs automatically after your run) and "
        "do NOT commit; leave the working tree with the candidate edit in place."
    )

    packet = f"""# Evo Candidate Dispatch Packet {seq}

Objective: {args.objective}
Hypothesis: {hypothesis}
Test command: `{args.test_cmd}`
Metric: {args.metric or 'n/a'} (higher-is-better={higher_is_better})
Baseline commit: {lineage.get('bestCommit')}

## Dispatch (run in the MCP client; the MCP server handles verify + lineage)

```js
qwen_coworker(
  prompt: {json.dumps(instruction)},
  session_id: "{session_id}",
  cwd: {json.dumps(target_dir)},
  hypothesis: {json.dumps(hypothesis)},
  test_command: {json.dumps(args.test_cmd)},
  metric_name: {json.dumps(args.metric)},
  higher_is_better: {str(higher_is_better).lower()},
)
```
"""
    with open(packet_path, "w", encoding="utf-8", newline="\n") as f:
        f.write(packet)

    print(f"--- Dispatch packet written: {packet_path} ---")
    print(packet)
    print(
        "Next: run the qwen_coworker call above in your MCP client. After the task "
        "completes, re-run this script - it will read the updated lineage and "
        "propose the next variation."
    )


if __name__ == "__main__":
    main()
