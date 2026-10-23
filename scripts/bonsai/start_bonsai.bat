@echo off
REM High-performance, zero-RAM-spill launcher for Ternary-Bonsai-2-27B on llama.cpp (PRISM CUDA).
REM Configured for 100% GPU VRAM residency, Flash Attention, Q8 KV cache, and 98%+ prefix cache hits.
REM Port: 18020 (Castor default)

setlocal

set BIN=.\llama-cpp\prism-cuda\llama-server.exe
set MODEL=.\models\Ternary-Bonsai-2-27B-PQ2_0.gguf
set PORT=18020
set CTX=65536
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
echo Starting Ternary-Bonsai-2-27B-PQ2_0 on llama.cpp (PRISM CUDA)
echo VRAM Footprint: ~10.6 GiB (6.7 GiB weights + 2.0 GiB Q8 KV Cache)
echo Context:        %CTX% tokens (1 slot, continuous LCP prefix caching)
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
