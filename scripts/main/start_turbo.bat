@echo off
echo Starting Qwen3.8-27B Turbo Specialist (DFlash2+KVarN, CTX=huge, 245k context, MAX_SEQS=2)...
wsl -d Ubuntu -- bash ~/qwen-serving/launchers/start_turbo.sh

