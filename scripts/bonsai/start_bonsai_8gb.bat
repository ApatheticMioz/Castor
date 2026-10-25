@echo off
REM 8GB VRAM profile: Single-Slot High-Efficiency Launcher for Ternary-Bonsai-2-27B.
REM Target hardware: 8GB cards (RTX 3070 8GB, RTX 3060 Ti 8GB, RTX 4060 8GB, RTX 4060 Ti 8GB).
REM
REM "SPECULATION OR CONTEXT?" ON 8GB:
REM   A neural drafter (DFlash2) consumes ~500 MB VRAM, which on an 8GB card collapses the safe
REM   context window to an unusable 2K-4K tokens. Context is paramount for agentic pair programming.
REM   Therefore, the 8GB profile chooses:
REM   1. Zero neural drafter overhead (saving 500 MB VRAM).
REM   2. Maximize safe context to 8,192 tokens (8K) in Q4_0 KV (~512 MB KV cache).
REM   3. Zero-VRAM Speculation: uses dynamic host-side n-gram lookup (-lcd) which accelerates
REM      code syntax, keywords, and variable repetitions with 0 MB of GPU memory penalty.
REM
REM VRAM BUDGET BREAKDOWN (~7.7 GB total on 8GB VRAM):
REM   Main weights (Ternary-Bonsai-2-27B-PQ2_0, 1.58-bit ternary)   6.70 GB
REM   DFlash2 drafter                                              0.00 GB (Omitted for context)
REM   KV cache (-c 8192 -ctk/-ctv q4_0, 64 KB/token)               0.51 GB
REM   CUDA context + activation / scratch buffers                  0.45 GB
REM   ----------------------------------------------------------------
REM   Total Net GPU Footprint                                      ~7.66 GB
REM   Safety Headroom                                              ~0.53 GB on 8GB (8,192 MiB)
REM
REM Stop with: stop_bonsai.bat

setlocal
cd /d "%~dp0\..\.."

set BIN=.\llama-cpp\prism-cuda\llama-server.exe
set MODEL=.\models\Ternary-Bonsai-2-27B-PQ2_0.gguf
set PORT=18020
set CTX=8192
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

echo ===================================================================
echo Starting Bonsai-27B 8GB PROFILE (8K Ctx, 1 Slot, Q4_0 KV, Dynamic Lookup)
echo Target:         %MODEL% (6.70 GiB, 1.58-bit ternary)
echo Drafter:        None (Zero-VRAM Dynamic N-gram Lookup via -lcd)
echo Context:        %CTX% tokens (1 dedicated slot, -np 1)
echo KV Cache:       Q4_0 (-ctk/-ctv q4_0, ~64 KB/token, ~512 MB at 8K)
echo Speculation:    Host Dynamic Lookup (-lcd .scratch\bonsai_dynamic_lookup.bin)
echo FlashAttention: ON (Micro-batch: %UBATCH%, Batch: %BATCH%)
echo VRAM Budget:    ~7.66 GB total (fits 100%% inside 8GB VRAM)
echo Reasoning:      DeepSeek format + History Preservation
echo Endpoint:       http://127.0.0.1:%PORT%/v1
echo ===================================================================

"%BIN%" ^
  -m "%MODEL%" ^
  -lcd .scratch\bonsai_dynamic_lookup.bin ^
  --host 127.0.0.1 ^
  --port %PORT% ^
  --alias qwen3.8-27b,Ternary-Bonsai-2-27B-PQ2_0 ^
  --jinja ^
  -ngl 99 ^
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
