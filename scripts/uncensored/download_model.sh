#!/bin/bash
# One-time provisioning: pulls the 1:1 symmetric uncensored Qwen3.8-27B model for HyperQwen.
# Model: noon-at-cgn/Qwen3.8-27B-Uncensored-W4A16-AutoRound
# (Intel AutoRound W4A16 Symmetric, 7 shards, zero zero-point penalty, exact 14.71 GiB parity)
#
# Target directory: ~/qwen-uncensored/model
#
# Invoke via:
#   MSYS_NO_PATHCONV=1 wsl -d Ubuntu -- bash /mnt/d/LLM_Ecosystem/scripts/uncensored/download_model.sh
set -e

MODEL_DIR="$HOME/qwen-uncensored/model"
REPO_ID="${1:-noon-at-cgn/Qwen3.8-27B-Uncensored-W4A16-AutoRound}"

mkdir -p "$MODEL_DIR"
cd "$MODEL_DIR"

source ~/qwen-serving/venv/bin/activate

echo "=========================================================="
echo "Downloading Symmetric Uncensored Qwen3.8-27B (AutoRound)"
echo "Repo:   $REPO_ID"
echo "Target: $MODEL_DIR"
echo "=========================================================="

python3 -c "
from huggingface_hub import snapshot_download
p = snapshot_download(repo_id='$REPO_ID', local_dir='.',
                      allow_patterns=['*.json', '*.jinja', '*.txt', '*.safetensors', 'README.md'])
print('Download complete at:', p)
"

# Also symlink into ~/qwen-serving/models/ so both paths resolve
mkdir -p ~/qwen-serving/models
ln -sfn "$MODEL_DIR" ~/qwen-serving/models/Qwen3.8-27B-Uncensored-W4A16-AutoRound
ln -sfn "$MODEL_DIR" ~/qwen-serving/models/Qwen3.8-27B-Uncensored-W4A16

echo "=========================================================="
echo "Download finished! Next, run the CPU requantization step:"
echo "  wsl -d Ubuntu -- bash /mnt/d/LLM_Ecosystem/scripts/uncensored/requantize.sh"
echo "=========================================================="
