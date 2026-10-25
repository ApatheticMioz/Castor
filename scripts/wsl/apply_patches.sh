#!/usr/bin/env bash
set -euo pipefail

SERVING_DIR="${HOME}/qwen-serving"
cd "$SERVING_DIR"

SP="venv/lib/python3.12/site-packages/vllm"

echo "=== Applying patches series via patches/apply.sh ==="
bash patches/apply.sh "$SP"

echo "=== Installing KVarN for vLLM ==="
bash kvarn/install.sh

echo "=== Installing FA2 fp8-KV plugin ==="
bash fa2/install.sh

echo "=== Installing pthfa attention plugin ==="
bash pthfa/install.sh

echo "=== Running verify.sh --no-server ==="
bash verify.sh --no-server
