#!/bin/bash
set -e
source "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/lib.sh"
castor_kill_stale
castor_resolve_dirs
export SPEC=dflash2
export CTX=huge
export PREFIX_CACHE=1
# Vision configuration: CPU-offloaded vision tower (VISION_OFFLOAD=1 pinned in host RAM).
export VISION=1
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:False
export VLLM_WSL2_ENABLE_PIN_MEMORY=1
export VLLM_DFLASH2_LOOKUP_ADAPTIVE=0
export EMBED_UVA=${EMBED_UVA:-1}
export MAX_SEQS=${MAX_SEQS:-4}
# Full HyperQwen 245,760 long-context configuration:
# MAX_SEQS=4 captures 32-query-token decode graphs (saving ~0.9 GiB vs MAX_SEQS=8),
# allowing the full 245,760 context and 268k token KV cache to fit directly inside 24GB VRAM.
export MAX_LEN=${MAX_LEN:-245760}
export KV_MEM=${KV_MEM:-5261334938}
export REQ_METRICS=1
# Server-side reasoning default: xhigh effort when no per-request override is sent.
export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"xhigh"}'
# Universal Stateful UTF-8 & SSE Stream Sanitizer Proxy (port 18022 -> 18020)
castor_spawn_proxy
exec bash single-user/start_qwen.sh
