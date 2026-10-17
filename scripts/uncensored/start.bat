@echo off
REM Default launcher: boots the huge config (245K context, ~77 tok/s) with Uncensored Qwen3.8-27B.
REM Mutually exclusive with scripts\main\ (both use port 18020 and full GPU). Stop main first.
echo Starting Uncensored Qwen3.8-27B (vLLM+DFlash2+KVarN, 245K context, reasoning ON)...
wsl -d Ubuntu -- bash /mnt/d/LLM_Ecosystem/scripts/wsl/start_uncensored.sh
