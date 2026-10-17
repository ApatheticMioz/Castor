@echo off
REM Fast latency launcher: boots the fast config (57K context, ~107-130 tok/s) with Uncensored Qwen3.8-27B.
echo Starting Uncensored Qwen3.8-27B (vLLM+DFlash2, CTX=fast, 57k context, ~120 tok/s)...
wsl -d Ubuntu -- bash /mnt/d/LLM_Ecosystem/scripts/wsl/start_uncensored_fast.sh
