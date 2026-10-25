@echo off
REM 12GB VRAM profile: Single-Slot DFlash2 Launcher for Ternary-Bonsai-2-27B.
REM Target hardware: 12GB cards (RTX 3060 12GB, RTX 4070 12GB).
REM
REM VRAM BUDGET BREAKDOWN (~11.7 GB total on 12GB VRAM):
REM   Main weights (Ternary-Bonsai-2-27B-PQ2_0, 1.58-bit ternary)   6.70 GB
REM   DFlash2 drafter (Bonsai-2-27B-DFlash2-Q8_0, Q8_0)            ~0.50 GB
REM   KV cache (-c 65536 -ctk/-ctv q4_0, 64 KB/token)              4.00 GB
REM   CUDA context + activation / scratch buffers                  0.50 GB
REM   ----------------------------------------------------------------
REM   Total                                                        ~11.70 GB
REM
REM Single dedicated slot (-np 1), 64K context, fused FlashAttention
REM (-fa -b 2048 -ub 512), lookup-augmented DFlash2 speculation with a
REM dynamic -lcd cache, and full GPU offload (-ngl 99 -ngld 99).
REM
REM Stop with: stop_bonsai.bat

setlocal
cd /d "%~dp0\..\.."

set BIN=.\llama-cpp\prism-cuda\llama-server.exe
set MODEL=.\models\Ternary-Bonsai-2-27B-PQ2_0.gguf
set DRAFT_FINE=.\models\Bonsai-2-27B-DFlash2-Q8_0.gguf
set DRAFT_FALLBACK=.\models\Qwen3.8-27B-DFlash2-Q4_K_M.gguf
set PORT=18020
set CTX=65536
set BATCH=2048
set UBATCH=512

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
echo Starting Bonsai-27B 12GB PROFILE (64K Ctx, 1 Slot, Q4_0 KV, DFlash2)
echo Target:         %MODEL% (6.70 GiB, 1.58-bit ternary)
echo Drafter:        %DRAFT%
echo Context:        %CTX% tokens (1 dedicated slot, -np 1)
echo KV Cache:       Q4_0 (-ctk/-ctv q4_0, ~64 KB/token, ~4.0 GB at 64K)
echo Speculation:    Lookup-Augmented DFlash2 (--spec-type draft-dflash,ngram-mod --spec-draft-n-max 5)
echo Lookup Cache:   Dynamic (-lcd .scratch\bonsai_dynamic_lookup.bin)
echo FlashAttention: ON (Micro-batch: %UBATCH%, Batch: %BATCH%)
echo VRAM Budget:    ~11.7 GB total (fits 100%% inside 12GB VRAM)
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
  --alias qwen3.8-27b,Ternary-Bonsai-2-27B-PQ2_0 ^
  --jinja ^
  -ngl 99 ^
  -ngld 99 ^
  -fa on ^
  -b %BATCH% ^
  -ub %UBATCH% ^
  -c %CTX% ^
  -np 1 ^
  -ctk q4_0 ^
  -ctv q4_0 ^
  -sps 0.05 ^
  --reasoning-format deepseek ^
  --reasoning-preserve

endlocal
