#!/bin/bash
# vLLM + DFlash2, CTX=fast: 57,344 context, ~107-130 tok/s, no KVarN.
# Fast latency mode for Uncensored Qwen3.8-27B.
set -e
source "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/lib.sh"
castor_kill_stale
castor_resolve_dirs

export MODEL="${MODEL:-$HOME/qwen-uncensored/model}"
export SPEC=dflash2
export DFLASH_TOKENS=15
export PREFIX_CACHE=1
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:False
export VLLM_WSL2_ENABLE_PIN_MEMORY=1

if [ -z "${EXTRA_ARGS:-}" ]; then
  export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"xhigh"}'
fi

castor_spawn_proxy
exec bash single-user/start_qwen.sh
