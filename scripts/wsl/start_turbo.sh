#!/bin/bash
# HyperQwen Turbo Specialist Launcher (Peak 1-User Burst, MAX_SEQS=2)
# Architecture: DFlash2 + KVarN 245k Context + Context Lookup Drafting
# Measured Upstream: 131 tok/s single-user, up to 381 tok/s on context quoting
set -e
# Common setup: kill stale vLLM/EngineCore, resolve dirs, spawn `castor proxy`.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
castor_kill_stale
castor_resolve_dirs

# Distinct turbo configuration (DFlash2 drafting, 2 streams, 245k ctx).
export SPEC=dflash2
export DFLASH_TOKENS=7
export CTX=huge
export PREFIX_CACHE=1
export VISION=1
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:False
export VLLM_WSL2_ENABLE_PIN_MEMORY=1
export VLLM_DFLASH2_LOOKUP_ADAPTIVE=0
export EMBED_UVA=${EMBED_UVA:-1}
export MAX_SEQS=${MAX_SEQS:-2}
export MAX_LEN=${MAX_LEN:-245760}
export KV_MEM=${KV_MEM:-5261334938}
export REQ_METRICS=1
export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"xhigh"}'

castor_spawn_proxy
exec bash single-user/start_qwen.sh
