#!/bin/bash
# vLLM + DFlash2 + KVarN, CTX=huge: 245,760 context, ~77 tok/s. DEFAULT config.
# Serves the 1:1 Symmetric Uncensored Qwen3.8-27B (AutoRound) via HyperQwen.
# Port: 18020 (sanitizer proxy on 18022 -> 18020).
# NOTE: Mutually exclusive with scripts/main/ (both need full 24 GB GPU).
set -e
pkill -9 -f 'vllm serve' 2>/dev/null || true
pkill -9 -f EngineCore 2>/dev/null || true
sleep 3
cd ~/qwen-serving

# Point to the uncensored model directory
export MODEL="${MODEL:-$HOME/qwen-uncensored/model}"

export SPEC=dflash2
export CTX=huge
export PREFIX_CACHE=1
export VISION=1
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:False
export VLLM_WSL2_ENABLE_PIN_MEMORY=1
export VLLM_DFLASH2_LOOKUP_ADAPTIVE=0
export MAX_SEQS=1
export REQ_METRICS=1

# Reasoning default: medium effort
if [ -z "${EXTRA_ARGS:-}" ]; then
  export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"medium"}'
fi

# Universal Stateful UTF-8 & SSE Stream Sanitizer Proxy (port 18022 -> 18020)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ECOSYSTEM_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CASTOR_BIN="${ECOSYSTEM_ROOT}/target/release/castor"
if [ ! -f "$CASTOR_BIN" ]; then
  CASTOR_BIN="${ECOSYSTEM_ROOT}/target/debug/castor"
fi
if [ -f "$CASTOR_BIN" ]; then
  pkill -9 -f 'castor proxy' 2>/dev/null || true
  setsid "$CASTOR_BIN" proxy < /dev/null > /tmp/stream_proxy.log 2>&1 &
fi
exec bash single-user/start_qwen.sh
