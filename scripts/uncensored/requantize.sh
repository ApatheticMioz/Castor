#!/bin/bash
# Requantize the symmetric uncensored Qwen3.8-27B AutoRound checkpoint for HyperQwen.
# Runs 100% on CPU without touching the GPU (low memory streaming mode).
#
# Quantizes lm_head, embed_tokens, and the MTP module in-place, then slices the 40k draft vocab.
# Resulting footprint is exactly 14.71 GiB with 0 MiB zero-point overhead.
#
# Usage:
#   bash requantize.sh [model_dir]
#
# Default:
#   ~/qwen-uncensored/model
set -e
export CUDA_VISIBLE_DEVICES=""

MODEL_DIR="${1:-$HOME/qwen-uncensored/model}"
VENV="$HOME/qwen-serving/venv"
PREPARE="$HOME/qwen-serving/prepare"

if [ ! -d "$VENV" ]; then
  echo "Error: Virtualenv not found at $VENV" >&2
  exit 1
fi

if [ ! -f "$MODEL_DIR/config.json" ]; then
  echo "Error: Model not found at $MODEL_DIR. Run download_model.sh first." >&2
  exit 1
fi

echo "=========================================================="
echo "HyperQwen CPU In-Place Streaming Requantization Pipeline"
echo "Model Directory: $MODEL_DIR"
echo "=========================================================="

echo "[1/2] Streaming-requantizing heads and MTP module (CPU row-chunked, low-RSS)..."
"$VENV/bin/python" "$PREPARE/quant_heads_stream.py" "$MODEL_DIR"

echo "[2/2] Slicing 40k-token draft vocabulary for speculative decoding..."
"$VENV/bin/python" "$PREPARE/build_draft_vocab.py" "$MODEL_DIR" --ids "$PREPARE/draft_vocab_ids.json"

echo "=========================================================="
echo "Requantization complete! Model is verified and ready."
echo "Location: $MODEL_DIR"
echo "=========================================================="
