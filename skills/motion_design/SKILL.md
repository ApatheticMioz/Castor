---
name: motion-design
description: Deterministic programmatic motion graphics pipeline — storyboard JSON, seeded math, pure draw(t), and Playwright+FFmpeg capture with entropy-based fitness.
keywords: [motion graphics, deterministic, canvas, ffmpeg, playwright, keyframes, entropy, fitness, animation, video]
---
# Motion Design Pipeline

A four-stage deterministic pipeline for producing programmatic motion graphics
(HTML5 Canvas / SVG → 60 fps MP4) with **zero** use of `Math.random()` or
wall-clock `Date.now()`. Every frame is a pure function of virtual time `t`.

## Stage 1 — Storyboard JSON Specification

Define the animation as a JSON document:

```json
{
  "timeline": { "fps": 60, "duration_s": 4.0 },
  "scenes": [
    {
      "id": "intro",
      "start_t": 0.0,
      "end_t": 2.0,
      "keyframes": [
        { "t": 0.0, "props": { "alpha": 0.0, "scale": 0.5 } },
        { "t": 1.0, "props": { "alpha": 1.0, "scale": 1.0 } }
      ]
    }
  ]
}
```

- `fps` is fixed at 60.
- `duration_s` is the total timeline length in seconds.
- Each scene has a `[start_t, end_t]` window and one or more keyframes.
- All times are in seconds of **virtual** time (never wall-clock).

## Stage 2 — Deterministic Math & Easing Primitives

All interpolation and randomness must be seeded and reproducible:

| Primitive | Rule |
|-----------|------|
| `lerp(a, b, u)` | `a + (b - a) * u` |
| `cubic-bezier(x1, y1, x2, y2)` | Pre-computed LUT (1024 samples); no iterative Newton at render time |
| `spring(stiffness, damping, u)` | Closed-form damped harmonic oscillator, no wall-clock |
| `seeded_prng(seed, frame)` | SplitMix64 / xorshift — one value per (seed, frame) pair, fully reproducible |

**Forbidden**: `Math.random()`, `Date.now()`, `performance.now()`, any OS-level RNG.

## Stage 3 — Pure `draw(t)` Scene Functions

Each scene is a **pure function** of virtual time:

```js
// canvas: HTML5 CanvasRenderingContext2D
// t: seconds of virtual time (0.0 .. duration_s)
function draw(canvas, t) {
  const ctx = canvas.getContext('2d');
  // All motion derived from t via Stage-2 primitives only.
}
```

- No side effects, no closures over mutable state.
- `t` is the **only** time parameter.
- In SVG mode the same function updates attributes on an SVG tree.

## Stage 4 — Capture, Encode, Verify

### 4a. Playwright Virtual-Clock Frame Capture
- Launch headless Chromium via Playwright.
- For `frame in 0..total_frames`: set virtual clock `t = frame / fps`, call `draw(t)`,
  capture `canvas.toDataURL('image/png')` (or SVG snapshot).
- Write each frame as `<out_dir>/frame_<NNNN>.png`.

### 4b. FFmpeg 60 fps MP4 Encoding
```bash
ffmpeg -y -framerate 60 -i <out_dir>/frame_%04d.png \
       -c:v libx264 -pix_fmt yuv420p -crf 18 <output_mp4>
```

### 4c. Hierarchical Multimodal Vision Verification
1. **Local (coworker `read_file`, $0 cost)**: Open keyframes (e.g. frames 0, mid, last)
   as images. Verify:
   - No all-black / all-white / blank canvas (entropy > threshold).
   - No `NaN` / `undefined` text rendered on canvas.
   - Keyframe geometry matches the storyboard spec (coordinate checks).
2. **Cloud Architect (aesthetic review)**: Send the MP4 (or 3–5 keyframes) to a
   vision model for aesthetic/continuity scoring. Only invoked when local checks pass.

### 4d. `score_frames` (Evo Gate)
- Computes byte-level Shannon entropy across all generated frame files.
- Rejects blank/degenerate outputs (entropy below threshold → `ok: false`).
- Returns a fitness score in `[0.0, 1.0]` usable as an `evo` selection criterion.
