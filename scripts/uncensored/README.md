# Uncensored Qwen3.8-27B (HyperQwen 1:1 Symmetric Stack)

This directory serves the **Uncensored (Abliterated) Qwen3.8-27B** model with **1:1 bit-for-bit parity** to the main aligned server:
* Same **Intel AutoRound W4A16 Symmetric** quantization (`symmetric=True`, zero zero-point penalty)
* Same **14.71 GiB** baseline weight footprint
* Full **245,760 context window** via KVarN 4/2-bit KV cache on `start.bat`
* Same **DFlash2** speculative acceleration (~77 tok/s on 245k context, ~120 tok/s on fast mode)
* Same **CPU-offloaded vision tower** (`VISION=1`) in host RAM (0 MiB VRAM overhead)
* **Zero refusals / de-refused** for deep binary and reverse engineering analysis.

---

## Launchers

* **`start.bat`**: Default launcher. Boots the huge config (245,760 context, KVarN 4/2-bit, DFlash2, medium reasoning effort).
* **`start_fast.bat`**: Low-latency launcher (57,344 context, ~107-130 tok/s).
* **`start_noreason.bat`**: Boots with `reasoning_effort: none` for direct answers without `<think>` traces.
* **`stop.bat`**: Stops any running vLLM server in WSL.

---

## Provisioning & Requantization

1. **`download_model.sh`**:
   Pulls `noon-at-cgn/Qwen3.8-27B-Uncensored-W4A16-AutoRound` (7 shards) into `~/qwen-uncensored/model`.
   ```bash
   wsl -d Ubuntu -- bash /mnt/d/LLM_Ecosystem/scripts/uncensored/download_model.sh
   ```

2. **`requantize.sh`**:
   Runs HyperQwen's CPU in-place head requantization (`quant_lm_head`, `quant_embed`, `quant_mtp`, `build_draft_vocab`). 100% CPU, 0% GPU.
   ```bash
   wsl -d Ubuntu -- bash /mnt/d/LLM_Ecosystem/scripts/uncensored/requantize.sh
   ```
