#!/usr/bin/env python3
"""
motion_mcp_server.py — Deterministic motion-graphics MCP server (stdio JSON-RPC 2.0).

Implements the MCP protocol subset used by `ExtensionBridge` (rmcp crate):
  - `initialize`
  - `notifications/initialized`
  - `tools/list`
  - `tools/call`

Tools:
  - `render_scene`: Steps virtual time, captures frames (Playwright or dry-run),
    encodes to MP4 via FFmpeg.
  - `score_frames`: Deterministic visual-fitness operator (entropy / NaN check).

Cross-platform: works on Linux, macOS, and Windows with `python3` or `python`.
"""

import sys
import json
import os
import math
import struct
import zlib
import hashlib
import subprocess
import tempfile
from pathlib import Path


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _write(msg: dict) -> None:
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def _ok(id, result: dict) -> None:
    _write({"jsonrpc": "2.0", "id": id, "result": result})


def _err(id, code: int, message: str) -> None:
    _write({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})


# ---------------------------------------------------------------------------
# render_scene
# ---------------------------------------------------------------------------

def _deterministic_keyframe_png(width: int, height: int, t: float, seed: int) -> bytes:
    """
    Generate a deterministic 24-bit frame as raw RGB bytes (no PNG
    compression — the caller writes the PNG container via `_write_raw_frame`).

    The pixel values are derived from a seeded PRNG + t so every frame is
    unique but fully reproducible.
    """
    # SplitMix64 seeded PRNG
    def _sm64(state: int) -> int:
        state = (state + 0x9E3779B97F4A7C15) & 0xFFFFFFFFFFFFFFFF
        z = state
        z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & 0xFFFFFFFFFFFFFFFF
        z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & 0xFFFFFFFFFFFFFFFF
        z = z ^ (z >> 31)
        return z

    state = (seed * 0x100000001B3 + int(t * 60)) & 0xFFFFFFFFFFFFFFFF
    pixels = bytearray(width * height * 3)
    for i in range(len(pixels) // 3):
        state = _sm64(state)
        r = (state >> 16) & 0xFF
        g = (state >> 8) & 0xFF
        b = state & 0xFF
        # Blend with a smooth gradient based on t to make frames visually distinct
        gx = (i % width) / width
        gy = (i // width) / height
        r = (r + int(60 * t)) & 0xFF
        g = (g + int(30 * gx * t * 60)) & 0xFF
        b = (b + int(20 * gy * t * 60)) & 0xFF
        pixels[i * 3] = r
        pixels[i * 3 + 1] = g
        pixels[i * 3 + 2] = b
    return bytes(pixels)


def _write_raw_frame(path: Path, w: int, h: int, rgb: bytes) -> None:
    """Write a valid, standards-compliant 24-bit PNG file.

    Layout: 8-byte signature + IHDR + IDAT (zlib-compressed raw scanlines)
    + IEND. Built purely from the stdlib `struct` and `zlib` modules — no
    third-party image library required.
    """

    def _chunk(tag: bytes, data: bytes) -> bytes:
        # length (big-endian) + tag + data + CRC32 over (tag + data).
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)

    # IHDR: width, height, bit-depth=8, color-type=2 (RGB), compression=0,
    # filter=0 (none), interlace=0.
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)

    # Build raw scanlines: each row prefixed with the "None" filter byte (0).
    raw = bytearray()
    for y in range(h):
        raw.append(0)  # filter type 0 (None) per scanline
        raw += rgb[y * w * 3:(y + 1) * w * 3]
    idat_data = zlib.compress(bytes(raw))

    # PNG signature: \x89PNG\r\n\x1a\n
    png = b"\x89PNG\r\n\x1a\n"
    png += _chunk(b"IHDR", ihdr)
    png += _chunk(b"IDAT", idat_data)
    png += _chunk(b"IEND", b"")
    path.write_bytes(png)


def _try_playwright_capture(html_path: str, total_frames: int, out_dir: Path, width: int, height: int) -> bool:
    """
    Attempt Playwright headless-Chromium capture. Returns True on success.
    Falls back to deterministic keyframe generation if Playwright is not available.
    """
    try:
        from playwright.sync_api import sync_playwright
    except ImportError:
        return False

    try:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            page = browser.new_page(viewport={"width": width, "height": height})
            page.goto(f"file://{os.path.abspath(html_path)}")
            page.wait_for_timeout(100)  # allow any JS to settle

            for frame in range(total_frames):
                t = frame / 60.0
                page.evaluate(f"window.__virtualT = {t}; if (typeof draw === 'function') draw(document.querySelector('canvas'), {t});")
                el = page.query_selector("canvas")
                if el is None:
                    # Fallback: screenshot
                    page.screenshot(path=str(out_dir / f"frame_{frame:04d}.png"))
                else:
                    el.screenshot(path=str(out_dir / f"frame_{frame:04d}.png"))
            browser.close()
        return True
    except Exception as e:
        print(f"[motion_mcp] playwright capture failed: {e}", file=sys.stderr)
        return False


def _try_ffmpeg_encode(out_dir: Path, output_mp4: str, fps: int) -> bool:
    """
    Encode frames to MP4 via FFmpeg. Returns True on success.
    """
    try:
        import shutil
        if shutil.which("ffmpeg") is None:
            return False
    except Exception:
        return False

    cmd = [
        "ffmpeg", "-y",
        "-framerate", str(fps),
        "-i", str(out_dir / "frame_%04d.png"),
        "-c:v", "libx264",
        "-pix_fmt", "yuv420p",
        "-crf", "18",
        output_mp4,
    ]
    try:
        result = subprocess.run(cmd, capture_output=True, timeout=120)
        return result.returncode == 0
    except Exception as e:
        print(f"[motion_mcp] ffmpeg failed: {e}", file=sys.stderr)
        return False


def _render_scene(args: dict) -> str:
    """
    render_scene tool implementation.

    Args:
        html_path: path to the HTML file (or a virtual name for dry-run)
        output_mp4: path to write the MP4
        fps: frames per second (default 60)
        duration_s: total duration in seconds
        keyframes_only: if True, only generate keyframe-count frames (for testing)
    """
    html_path = args.get("html_path", "")
    output_mp4 = args.get("output_mp4", "out.mp4")
    fps = int(args.get("fps", 60))
    duration_s = float(args.get("duration_s", 2.0))
    keyframes_only = bool(args.get("keyframes_only", False))

    total_frames = int(duration_s * fps) if not keyframes_only else max(1, int(duration_s * 2))
    width = 640
    height = 360
    seed = 42

    out_dir = Path(tempfile.mkdtemp(prefix="motion_frames_"))

    # Try Playwright first; fall back to deterministic keyframe generation
    used_playwright = False
    html_exists = html_path and Path(html_path).is_file()
    if html_exists:
        used_playwright = _try_playwright_capture(html_path, total_frames, out_dir, width, height)

    if not used_playwright:
        # Deterministic keyframe generation (dry-run / test mode).
        # Each frame is a real, standards-compliant PNG (IHDR + zlib IDAT + IEND).
        for frame in range(total_frames):
            t = frame / fps
            rgb = _deterministic_keyframe_png(width, height, t, seed)
            _write_raw_frame(out_dir / f"frame_{frame:04d}.png", width, height, rgb)

    # Try FFmpeg encode
    encoded = _try_ffmpeg_encode(out_dir, output_mp4, fps)

    result = {
        "ok": True,
        "frames_generated": total_frames,
        "playwright_used": used_playwright,
        "ffmpeg_encoded": encoded,
        "frame_dir": str(out_dir),
        "output_mp4": output_mp4,
        "output_exists": Path(output_mp4).is_file() if encoded else False,
    }
    return json.dumps(result)


# ---------------------------------------------------------------------------
# score_frames
# ---------------------------------------------------------------------------

def _shannon_entropy(data: bytes) -> float:
    """Compute Shannon entropy of a byte sequence in bits per byte (0..8)."""
    if not data:
        return 0.0
    freq = [0] * 256
    for b in data:
        freq[b] += 1
    n = len(data)
    entropy = 0.0
    for f in freq:
        if f > 0:
            p = f / n
            entropy -= p * math.log2(p)
    return entropy


def _score_frames(args: dict) -> str:
    """
    score_frames tool: deterministic visual-fitness operator.

    Args:
        frame_dir: directory containing frame files (frame_0000.png, etc.)
        or
        frame_files: explicit list of file paths
    """
    frame_dir = args.get("frame_dir", "")
    frame_files = args.get("frame_files", [])

    files: list = []
    if frame_files:
        files = [Path(f) for f in frame_files]
    elif frame_dir:
        d = Path(frame_dir)
        if d.is_dir():
            files = sorted(d.glob("frame_*"))
            if not files:
                files = sorted(d.glob("*"))

    if not files:
        return json.dumps({"ok": False, "fitness": 0.0, "entropy": 0.0,
                           "nan_free": False, "reason": "no frames found"})

    total_bytes = 0
    total_entropy_sum = 0.0
    nan_free = True
    blank_detected = False

    for f in files:
        if not f.is_file():
            continue
        data = f.read_bytes()
        if not data:
            blank_detected = True
            continue
        total_bytes += len(data)
        total_entropy_sum += _shannon_entropy(data)
        # Check for NaN in raw bytes (NaN in IEEE754 or literal text)
        if b"NaN" in data or b"undefined" in data:
            nan_free = False

    avg_entropy = (total_entropy_sum / len(files)) if files else 0.0
    # Entropy is in bits-per-byte (0..8). Normalize to 0..1.
    normalized = avg_entropy / 8.0

    if blank_detected:
        return json.dumps({"ok": False, "fitness": 0.0, "entropy": avg_entropy,
                           "nan_free": nan_free, "reason": "blank frame detected"})

    if not nan_free:
        return json.dumps({"ok": False, "fitness": 0.0, "entropy": avg_entropy,
                           "nan_free": False, "reason": "NaN or undefined in frame data"})

    # Degenerate: entropy < 0.1 bits/byte → nearly uniform/blank.
    # `normalized` is avg_entropy/8.0, so 0.1 bits/byte == 0.1/8.0 normalized.
    if normalized < 0.1 / 8.0:
        return json.dumps({"ok": False, "fitness": 0.0, "entropy": avg_entropy,
                           "nan_free": True, "reason": "degenerate: near-zero entropy"})

    fitness = round(min(1.0, normalized * 0.9 + 0.1), 4)
    return json.dumps({"ok": True, "fitness": fitness, "entropy": round(avg_entropy, 4),
                       "nan_free": True})


# ---------------------------------------------------------------------------
# MCP handler
# ---------------------------------------------------------------------------

def handle(msg: dict) -> None:
    id = msg.get("id")
    method = msg.get("method")
    params = msg.get("params") or {}

    if method == "initialize":
        _ok(id, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "motion-mcp", "version": "1.0.0"},
        })
        return

    if method == "notifications/initialized":
        return

    if method == "tools/list":
        _ok(id, {
            "tools": [
                {
                    "name": "render_scene",
                    "description": "Render a motion-graphics scene: step virtual time, capture frames (Playwright or deterministic dry-run), encode to 60fps MP4 via FFmpeg.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "html_path": {"type": "string", "description": "Path to the HTML scene file (or omit for dry-run)"},
                            "output_mp4": {"type": "string", "description": "Output MP4 path"},
                            "fps": {"type": "integer", "description": "Frames per second (default 60)"},
                            "duration_s": {"type": "number", "description": "Total duration in seconds"},
                            "keyframes_only": {"type": "boolean", "description": "If true, only generate keyframe-count frames"}
                        },
                        "required": ["output_mp4", "duration_s"]
                    },
                },
                {
                    "name": "score_frames",
                    "description": "Deterministic visual-fitness operator: checks for NaN/blank and computes Shannon entropy across frame files.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "frame_dir": {"type": "string", "description": "Directory containing frame files"},
                            "frame_files": {"type": "array", "items": {"type": "string"},
                                            "description": "Explicit list of frame file paths"}
                        }
                    },
                },
            ]
        })
        return

    if method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "render_scene":
            result = _render_scene(args)
        elif name == "score_frames":
            result = _score_frames(args)
        else:
            _err(id, -32601, f"unknown tool: {name}")
            return
        _ok(id, {"content": [{"type": "text", "text": result}], "isError": False})
        return

    if id is not None:
        _err(id, -32601, f"method not found: {method}")


def main() -> None:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except Exception:
            continue
        handle(msg)


if __name__ == "__main__":
    main()
