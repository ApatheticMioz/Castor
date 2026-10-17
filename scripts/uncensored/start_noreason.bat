@echo off
REM Uncensored Qwen3.8-27B with REASONING OFF (direct responses, zero thinking tokens).
echo Starting Uncensored Qwen3.8-27B via HyperQwen (REASONING OFF)...
wsl -d Ubuntu -- bash /mnt/d/LLM_Ecosystem/scripts/wsl/start_uncensored_noreason.sh
