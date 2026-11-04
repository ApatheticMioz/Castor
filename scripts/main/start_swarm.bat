@echo off
echo Starting Qwen3.8-27B Multi-Agent Swarm (MTP+PTH_FA, CTX=long, 150k context, MAX_SEQS=4)...
wsl -d Ubuntu -- bash ~/qwen-serving/launchers/start_swarm.sh

