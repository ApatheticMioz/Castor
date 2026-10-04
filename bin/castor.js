#!/usr/bin/env node
import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(__dirname, "..");
const isWin = process.platform === "win32";
const args = process.argv.slice(2);

const winReleaseExe = path.join(root, "target", "release", "castor.exe");
const winDebugExe = path.join(root, "target", "debug", "castor.exe");
const linuxReleaseBin = path.join(root, "target", "release", "castor");
const linuxDebugBin = path.join(root, "target", "debug", "castor");

function handleSpawn(child) {
  child.on("error", (err) => {
    if (err.code === "ENOENT") {
      console.error(
        `\x1b[31m[Castor Error]\x1b[0m Castor binary not found.\n\n` +
        `To build or install Castor:\n` +
        `  - Build from source: cargo build --release\n` +
        `  - Install to PATH:   cargo install --path .\n` +
        `  - Releases & Docs:   https://github.com/ApatheticMioz/Castor\n`
      );
      process.exit(1);
    }
    console.error(err);
    process.exit(1);
  });
  child.on("exit", (code, signal) => {
    if (signal) process.kill(process.pid, signal);
    process.exit(code ?? 1);
  });
}

function pickNewest(paths) {
  let newest = null;
  let maxTime = -1;
  for (const p of paths) {
    try {
      const stat = fs.statSync(p);
      if (stat.mtimeMs > maxTime) {
        maxTime = stat.mtimeMs;
        newest = p;
      }
    } catch {}
  }
  return newest;
}

if (isWin) {
  const winBin = pickNewest([winReleaseExe, winDebugExe]);
  if (winBin) {
    handleSpawn(spawn(winBin, args, { stdio: "inherit" }));
  } else {
    // Forward to WSL2
    const wslRoot = root
      .replace(/^([a-zA-Z]):/, (_, drive) => `/mnt/${drive.toLowerCase()}`)
      .replace(/\\/g, "/");

    const linuxBin = pickNewest([linuxReleaseBin, linuxDebugBin]);
    const wslBin = linuxBin
      ? `${wslRoot}/target/${linuxBin.includes("release") ? "release" : "debug"}/castor`
      : "castor";

    handleSpawn(
      spawn("wsl.exe", ["--", wslBin, ...args], {
        stdio: "inherit",
        windowsHide: true,
      })
    );
  }
} else {
  const bin = pickNewest([linuxReleaseBin, linuxDebugBin]) ?? "castor";
  handleSpawn(spawn(bin, args, { stdio: "inherit" }));
}

