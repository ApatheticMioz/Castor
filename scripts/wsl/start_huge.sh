#!/bin/bash
# vLLM + DFlash2 + KVarN, CTX=huge: 245,760 context, ~77 tok/s. DEFAULT config.
set -e
pkill -9 -f 'vllm serve' 2>/dev/null || true
pkill -9 -f EngineCore 2>/dev/null || true
sleep 3
cd ~/qwen-serving
export SPEC=dflash2
export CTX=huge
export PREFIX_CACHE=1
# Vision configuration: CPU-offloaded vision tower (VISION_OFFLOAD=1 pinned in host RAM).
export VISION=1
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:False
export VLLM_WSL2_ENABLE_PIN_MEMORY=1
export VLLM_DFLASH2_LOOKUP_ADAPTIVE=0
export EMBED_UVA=${EMBED_UVA:-1}
export MAX_SEQS=${MAX_SEQS:-1}
export REQ_METRICS=1
# Server-side reasoning default: medium effort when no per-request override is sent.
export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"medium"}'
# Universal Stateful UTF-8 & SSE Stream Sanitizer Proxy (port 18022 -> 18020)
# Derive the proxy binary from this script's own location so the repo is
# portable (no hardcoded /mnt/d/ drive mount).
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

