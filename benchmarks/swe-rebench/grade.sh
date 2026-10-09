#!/bin/bash
# Grade a predictions file against nebius/SWE-rebench-leaderboard using
# SWE-rebench's OWN eval tool (scripts/eval.py from SWE-rebench/SWE-rebench-V2,
# cloned to ~/swe-rebench-eval/repo). CORRECTED 2026-08-24: the generic PyPI
# `swebench` package's run_evaluation does NOT work against this dataset -
# it expects a pre-baked `eval_script`/`eval_type`/`log_parser` triple per
# instance (a newer/different harness API), while this dataset ships
# `install_config` + `harbor_*` fields for SWE-rebench's own tool instead.
# Confirmed by reading eval.py directly: it reads `image_name` (not `image`),
# builds the eval script itself from `install_config.test_cmd` +
# `install_config.log_parser`, and applies `patch`/`test_patch` inside the
# container - no Docker image build step needed, just `docker pull` of the
# already-built per-instance image.
#
# Usage: bash grade.sh predictions/archived_2026_03/castor_qwen_2026_03_50.jsonl [split]
set -e
PRED_FILE="${1:?usage: grade.sh <predictions.jsonl> [split]}"
SPLIT="${2:-2026_03}"
RUN_ID="$(basename "$PRED_FILE" .jsonl)"
PRED_DIR="$(cd "$(dirname "$PRED_FILE")" && pwd)"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
EVAL_REPO="$HOME/swe-rebench-eval/repo"
VENV="$HOME/swe-rebench-eval/venv/bin/python"

# Determine output report directory based on input directory
if [[ "$PRED_DIR" == *"/pilots"* ]]; then
    REPORT_DIR="$HERE/results/pilots"
elif [[ "$PRED_DIR" == *"/corpus_b"* ]]; then
    REPORT_DIR="$HERE/results/corpus_b"
elif [[ "$PRED_DIR" == *"/archived_2026_03"* ]]; then
    REPORT_DIR="$HERE/results/archived_2026_03"
else
    REPORT_DIR="$HERE/results"
fi
mkdir -p "$REPORT_DIR"

PATCH_FILE="${PRED_DIR}/patches_${RUN_ID}.json"

# Convert our {instance_id, model_patch, ...} JSONL to eval.py's expected
# [{"instance_id":..., "patch":...}, ...] JSON list.
"$VENV" -c "
import json
rows = []
with open('$PRED_FILE', encoding='utf-8') as f:
    for line in f:
        r = json.loads(line)
        rows.append({'instance_id': r['instance_id'], 'patch': r['model_patch']})
with open('$PATCH_FILE', 'w', encoding='utf-8') as f:
    json.dump(rows, f, ensure_ascii=False, indent=2)
print(f'Wrote {len(rows)} patch entries to $PATCH_FILE')
"

# --instance-ids scopes eval.py to exactly the sampled instances
INSTANCE_IDS=$("$VENV" -c "
import json
with open('$PRED_FILE', encoding='utf-8') as f:
    ids = [json.loads(l)['instance_id'] for l in f if l.strip()]
print(','.join(ids))
")

cd "$EVAL_REPO"
"$VENV" scripts/eval.py \
    --hf-dataset nebius/SWE-rebench-leaderboard \
    --hf-config default \
    --hf-split "$SPLIT" \
    --patches "$PATCH_FILE" \
    --instance-ids "$INSTANCE_IDS" \
    --max-workers 4 \
    --report-json "$REPORT_DIR/${RUN_ID}_report.json"

echo "Report written to $REPORT_DIR/${RUN_ID}_report.json"

