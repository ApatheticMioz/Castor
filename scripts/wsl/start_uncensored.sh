#!/bin/bash
# vLLM + DFlash2 + KVarN, CTX=huge: 245,760 context.
# Serves the 1:1 Symmetric Uncensored Qwen3.8-27B (AutoRound) via HyperQwen on port 18020.
set -e
source "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/lib.sh"
castor_kill_stale
castor_resolve_dirs

# Point to the uncensored model directory
export MODEL="${MODEL:-$HOME/qwen-uncensored/model}"

export SPEC=dflash2
export CTX=huge
export PREFIX_CACHE=1
export VISION=1
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:False
export VLLM_WSL2_ENABLE_PIN_MEMORY=1
export VLLM_DFLASH2_LOOKUP_ADAPTIVE=0
export EMBED_UVA=${EMBED_UVA:-1}
export MAX_SEQS=${MAX_SEQS:-1}
export REQ_METRICS=1

# Reasoning default: xhigh effort
if [ -z "${EXTRA_ARGS:-}" ]; then
  export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"xhigh"}'
fi

# Universal Stateful UTF-8 & SSE Stream Sanitizer Proxy (port 18022 -> 18020)
castor_spawn_proxy
exec bash single-user/start_qwen.sh
