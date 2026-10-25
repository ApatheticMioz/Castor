@echo off
REM High-end 24GB profile: Max-VRAM / 512K Unified Multi-Agent Launcher for Ternary-Bonsai-2-27B.
REM Allocates a 524,288-token unified hardware KV cache pool (-c 524288 -kvu) across 4 slots (-np 4)
REM with Q4_0 FlashAttention KV compression (16 KiB/tok), 16GB pageable host RAM cache tier (--cache-ram 16384),
REM and fine-tuned DFlash2 drafter. Completely eliminates multi-agent KV cache thrashing on mega-context tasks.

setlocal
cd /d "%~dp0\..\.."

set BIN=.\llama-cpp\prism-cuda\llama-server.exe
set MODEL=.\models\Ternary-Bonsai-2-27B-PQ2_0.gguf
set DRAFT_FINE=.\models\Bonsai-2-27B-DFlash2-Q8_0.gguf
set DRAFT_FALLBACK=.\models\Qwen3.8-27B-DFlash2-Q4_K_M.gguf
set PORT=18020
set SLOTS=4
set CTX=524288
set BATCH=2048
set UBATCH=2048
set CACHE_RAM=16384

if not exist "%BIN%" (
    echo [ERROR] PRISM CUDA llama-server binary not found at %BIN%
    exit /b 1
)

if not exist "%MODEL%" (
    echo [ERROR] Model weights not found at %MODEL%
    exit /b 1
)

if exist "%DRAFT_FINE%" (
    set DRAFT=%DRAFT_FINE%
    echo [DRAFTER] Using fine-tuned Bonsai-2-27B-DFlash2-Q8_0 drafter
) else (
    set DRAFT=%DRAFT_FALLBACK%
    echo [DRAFTER] Fallback to generic Qwen3.8-DFlash2-Q4_K_M drafter
)

if not exist "%DRAFT%" (
    echo [ERROR] No valid DFlash drafter weights found!
    exit /b 1
)

echo ===================================================================
echo Starting Bonsai-27B HIGH-END 24GB PROFILE (512K Unified Pool, 4 Slots, Q4_0 KV)
echo Target:         %MODEL% (6.70 GiB, 1.58-bit ternary)
echo Drafter:        %DRAFT%
echo Slots:          %SLOTS% dynamic slots (-np %SLOTS%)
echo Unified Pool:   %CTX% tokens (-kvu, shared dynamically across slots)
echo Host RAM Tier:  %CACHE_RAM% MB pageable virtual commit
echo KV Cache:       Q4_0 (16 KiB/token, Fused FlashAttention CUDA kernel)
echo Speculation:    Lookup-Augmented DFlash2 (--spec-type draft-dflash,ngram-mod --spec-draft-n-max 5)
echo Lookup Cache:   Dynamic (-lcd .scratch\bonsai_dynamic_lookup.bin)
echo FlashAttention: ON (Micro-batch: %UBATCH%, Batch: %BATCH%)
echo Reasoning:      DeepSeek format + History Preservation
echo Endpoint:       http://127.0.0.1:%PORT%/v1
echo ===================================================================

"%BIN%" ^
  -m "%MODEL%" ^
  -md "%DRAFT%" ^
  --spec-type draft-dflash,ngram-mod ^
  --spec-draft-n-max 5 ^
  -lcd .scratch\bonsai_dynamic_lookup.bin ^
  --host 127.0.0.1 ^
  --port %PORT% ^
  --alias qwen3.8-27b ^
  --alias Ternary-Bonsai-2-27B-PQ2_0 ^
  --jinja ^
  -ngl 99 ^
  -ngld 99 ^
  -fa on ^
  -b %BATCH% ^
  -ub %UBATCH% ^
  -c %CTX% ^
  -kvu ^
  -np %SLOTS% ^
  -ctk q4_0 ^
  -ctv q4_0 ^
  --cache-ram %CACHE_RAM% ^
  -sps 0.05 ^
  --reasoning-format deepseek ^
  --reasoning-preserve

endlocal
