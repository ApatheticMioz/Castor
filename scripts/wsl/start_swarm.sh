#!/bin/bash
# HyperQwen Multi-Agent Swarm Launcher (True N=4 Concurrency)
# Architecture: Qwen3.8-27B-W4A16-AutoRound-fast + MTP (k=3) + CTX=long (150k)
# Measured Upstream: 280.9 tok/s aggregate @ C4 (~70 tok/s per stream)
set -e
# Common setup: kill stale vLLM/EngineCore, resolve dirs, spawn `castor proxy`.
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
castor_kill_stale
castor_resolve_dirs

# Distinct swarm configuration (MTP speculative decoding, 4 streams, 150k ctx).
export SPEC=mtp
export DRAFT_TOKENS=3
export CTX=long
export PTH_FA=1
export MAX_SEQS=${MAX_SEQS:-4}
export MAX_LEN=${MAX_LEN:-150000}
export PREFIX_CACHE=1
export VISION=1
export EMBED_UVA=${EMBED_UVA:-1}
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:False
export VLLM_WSL2_ENABLE_PIN_MEMORY=1
export REQ_METRICS=1
export EXTRA_ARGS='--default-chat-template-kwargs {"reasoning_effort":"xhigh"}'

castor_spawn_proxy
exec bash single-user/start_qwen.sh
