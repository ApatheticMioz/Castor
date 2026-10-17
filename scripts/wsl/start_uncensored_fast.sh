#!/bin/bash
# vLLM + DFlash2, CTX=fast: 57,344 context, ~107-130 tok/s, no KVarN.
# Fast latency mode for Uncensored Qwen3.8-27B.
set -e
pkill -9 -f 'vllm serve' 2>/dev/null || true
pkill -9 -f EngineCore 2>/dev/null || true
sleep 3
cd ~/qwen-serving

export MODEL="${MODEL:-$HOME/qwen-uncensored/model}"
export SPEC=dflash2
export DFLASH_TOKENS=15
export PREFIX_CACHE=1
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:False
export VLLM_WSL2_ENABLE_PIN_MEMORY=1

if [ -z "${EXTRA_ARGS:-}" ]; then
  export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"medium"}'
fi

pkill -9 -f 'stream_proxy.js' 2>/dev/null || true
setsid node ~/qwen-serving/stream_proxy.js < /dev/null > /tmp/stream_proxy.log 2>&1 &
exec bash single-user/start_qwen.sh
