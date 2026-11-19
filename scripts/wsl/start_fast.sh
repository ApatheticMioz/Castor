#!/bin/bash
# vLLM + DFlash2, CTX=fast: 57,344 context, ~107-130 tok/s.
set -e
source "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/lib.sh"
castor_kill_stale
castor_resolve_dirs
export SPEC=dflash2
export DFLASH_TOKENS=15
export PREFIX_CACHE=1
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:False
export VLLM_WSL2_ENABLE_PIN_MEMORY=1
# Server-side reasoning default: xhigh effort when no per-request override is sent.
export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"xhigh"}'
# Universal Stateful UTF-8 & SSE Stream Sanitizer Proxy (port 18022 -> 18020)
castor_spawn_proxy
exec bash single-user/start_qwen.sh
