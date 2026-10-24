@echo off
REM High-context (131,072 tokens) launcher for Ternary-Bonsai-2-27B on llama.cpp (PRISM CUDA).
REM Uses Q8 KV cache (4.0 GiB) or Q4 KV cache (2.0 GiB). Total VRAM fits easily in 24GB.

setlocal
cd /d "%~dp0\..\.."

set BIN=.\llama-cpp\prism-cuda\llama-server.exe
set MODEL=.\models\Ternary-Bonsai-2-27B-PQ2_0.gguf
set PORT=18020
set CTX=131072
set BATCH=4096
set UBATCH=2048

if not exist "%BIN%" (
    echo [ERROR] PRISM CUDA llama-server binary not found at %BIN%
    exit /b 1
)

if not exist "%MODEL%" (
    echo [ERROR] Model weights not found at %MODEL%
    exit /b 1
)

echo ===================================================================
echo Starting Ternary-Bonsai-2-27B-PQ2_0 (128K Context Mode)
echo VRAM Footprint: ~10.7 GiB weights+KV (leaves >13 GiB free headroom)
echo Context:        131,072 tokens (Single slot, Q8_0 KV cache)
echo FlashAttention: ON (Micro-batch: %UBATCH%, Batch: %BATCH%)
echo Reasoning:      DeepSeek format + History Preservation
echo Endpoint:       http://127.0.0.1:%PORT%/v1
echo ===================================================================

"%BIN%" ^
  -m "%MODEL%" ^
  --host 127.0.0.1 ^
  --port %PORT% ^
  --alias Qwen3.8-27B ^
  --alias Ternary-Bonsai-2-27B-PQ2_0 ^
  --jinja ^
  -ngl 99 ^
  -fa on ^
  -b %BATCH% ^
  -ub %UBATCH% ^
  -c %CTX% ^
  -np 1 ^
  -ctk q8_0 ^
  -ctv q8_0 ^
  -sps 0.05 ^
  --reasoning-format deepseek ^
  --reasoning-preserve

endlocal
