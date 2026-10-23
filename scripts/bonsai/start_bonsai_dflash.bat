@echo off
REM High-performance, speculative-decoding launcher for Ternary-Bonsai-2-27B with DFlash2 block diffusion drafter.
REM Uses native PRISM CUDA DFlash implementation with tuned n_max=3 for minimal rejection penalty and 75+ t/s code bursts.

setlocal

set BIN=.\llama-cpp\prism-cuda\llama-server.exe
set MODEL=.\models\Ternary-Bonsai-2-27B-PQ2_0.gguf
set DRAFT=.\models\Qwen3.6-27B-DFlash-Q8_0.gguf
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

if not exist "%DRAFT%" (
    echo [ERROR] DFlash drafter weights not found at %DRAFT%
    exit /b 1
)

echo ===================================================================
echo Starting Ternary-Bonsai-2-27B with DFlash2 Block Diffusion Speculation
echo VRAM Footprint: ~17.6 GiB (6.7 GiB target + 1.84 GiB drafter + KV)
echo Context:        %CTX% tokens (1 slot, continuous LCP prefix caching)
echo Speculation:    DFlash2 (--spec-type draft-dflash --spec-draft-n-max 3)
echo FlashAttention: ON (Micro-batch: %UBATCH%, Batch: %BATCH%)
echo Reasoning:      DeepSeek format + History Preservation
echo Endpoint:       http://127.0.0.1:%PORT%/v1
echo ===================================================================

"%BIN%" ^
  -m "%MODEL%" ^
  -md "%DRAFT%" ^
  --spec-type draft-dflash ^
  --spec-draft-n-max 3 ^
  --host 127.0.0.1 ^
  --port %PORT% ^
  --alias Qwen3.8-27B ^
  --alias Ternary-Bonsai-2-27B-PQ2_0 ^
  --jinja ^
  -ngl 99 ^
  -ngld 99 ^
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
