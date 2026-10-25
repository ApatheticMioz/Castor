@echo off
REM 16GB VRAM profile: Single-Slot DFlash2 Launcher for Ternary-Bonsai-2-27B.
REM Target hardware: 16GB cards (RTX 4080 16GB, RTX 4070 Ti Super 16GB, RTX 4060 Ti 16GB, RX 7800 XT 16GB).
REM
REM "SPECULATION OR CONTEXT?" ON 16GB:
REM   On a 16GB GPU, you do NOT have to compromise between context and speculative decoding.
REM   1. Full 128K context (131,072 tokens) in Q4_0 KV consumes ~8.0 GB VRAM.
REM   2. The fine-tuned DFlash2 speculator consumes ~0.5 GB VRAM.
REM   3. Total measured VRAM is ~13.28 GB (13,278 MiB live), leaving >3.1 GB of safety headroom.
REM   4. Delivers ~53.0 tok/s decode across 128K context with ~36%% DFlash2 acceptance.
REM
REM VRAM BUDGET BREAKDOWN (~13.28 GB total on 16GB VRAM):
REM   Main weights (Ternary-Bonsai-2-27B-PQ2_0, 1.58-bit ternary)   6.70 GB
REM   DFlash2 drafter (Bonsai-2-27B-DFlash2-Q8_0, Q8_0)            ~0.50 GB
REM   KV cache (-c 131072 -ctk/-ctv q4_0, 64 KB/token)             8.00 GB
REM   CUDA context + activation / scratch buffers                  0.50 GB
REM   ----------------------------------------------------------------
REM   Total Net GPU Footprint                                      ~13.28 GB
REM   Safety Headroom                                              ~3.10 GB on 16GB (16,384 MiB)
REM
REM Stop with: stop_bonsai.bat

setlocal
cd /d "%~dp0\..\.."

set BIN=.\llama-cpp\prism-cuda\llama-server.exe
set MODEL=.\models\Ternary-Bonsai-2-27B-PQ2_0.gguf
set DRAFT_FINE=.\models\Bonsai-2-27B-DFlash2-Q8_0.gguf
set DRAFT_FALLBACK=.\models\Qwen3.8-27B-DFlash2-Q4_K_M.gguf
set PORT=18020
set CTX=131072
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
echo Starting Bonsai-27B 16GB PROFILE (128K Ctx, 1 Slot, Q4_0 KV, DFlash2)
echo Target:         %MODEL% (6.70 GiB, 1.58-bit ternary)
echo Drafter:        %DRAFT%
echo Context:        %CTX% tokens (1 dedicated slot, -np 1)
echo KV Cache:       Q4_0 (-ctk/-ctv q4_0, ~64 KB/token, ~8.0 GB at 128K)
echo Speculation:    Lookup-Augmented DFlash2 (--spec-type draft-dflash,ngram-mod --spec-draft-n-max 5)
echo Lookup Cache:   Dynamic (-lcd .scratch\bonsai_dynamic_lookup.bin)
echo FlashAttention: ON (Micro-batch: %UBATCH%, Batch: %BATCH%)
echo VRAM Budget:    ~13.28 GB total, >3.1 GB headroom on 16GB VRAM
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
