# Shared setup for the WSL serving launchers (start_swarm.sh, start_turbo.sh).
#
# The common parts every launcher needs, factored out so each launcher sources
# this file and supplies only its own distinct environment variables:
#   1. Kill any stale vLLM / EngineCore (and prior `castor proxy`) processes,
#      then wait for GPU + port teardown to settle.
#   2. Resolve the serving directory (default `$HOME/qwen-serving`,
#      env-overridable) and the repo root, changing into the serving dir so the
#      launcher's own entrypoint (`single-user/start_qwen.sh`) resolves.
#   3. Resolve the `castor` binary (release, then debug) from the repo and
#      spawn `castor proxy` (the loopback SSE stream proxy) detached.
#
# `lib.sh` is a sourced file, not an executable: it carries no `set -e` and no
# `exec`, so the sourcing launcher keeps control of its own error policy and
# terminal command.

# Default for the `castor proxy` log; launchers may override via their own env.
CASTOR_PROXY_LOG="${CASTOR_PROXY_LOG:-/tmp/stream_proxy.log}"

# Kill the old vLLM serving processes (and any prior `castor proxy`) and wait
# for teardown to settle. Tolerant of a clean host (no matches) — never fatal.
castor_kill_stale() {
  pkill -9 -f 'vllm serve' 2>/dev/null || true
  pkill -9 -f EngineCore 2>/dev/null || true
  pkill -9 -f 'castor proxy' 2>/dev/null || true
  sleep 3
}

# Resolve the serving directory and the repo root, then change into the
# serving dir. The serving dir defaults to `$HOME/qwen-serving` (env-driven,
# overridable via $QWEN_SERVING_DIR); the repo root is derived from this
# script's own location (lib.sh lives in scripts/wsl/, so its grandparent is
# the repo root) so the checkout is portable with no hardcoded drive letters.
castor_resolve_dirs() {
  local script_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  ECOSYSTEM_ROOT="$(cd "${script_dir}/../.." && pwd)"
  QWEN_SERVING_DIR="${QWEN_SERVING_DIR:-$HOME/qwen-serving}"
  cd "$QWEN_SERVING_DIR" || return 1
}

# Resolve the `castor` binary (release, then debug) from the repo root, and
# spawn `castor proxy` detached. No-op when no built binary is present, so a
# fresh checkout (pre-build) still launches the engine.
castor_spawn_proxy() {
  local castor_bin="${ECOSYSTEM_ROOT}/target/release/castor"
  [ -f "$castor_bin" ] || castor_bin="${ECOSYSTEM_ROOT}/target/debug/castor"
  if [ -f "$castor_bin" ]; then
    setsid "$castor_bin" proxy < /dev/null > "$CASTOR_PROXY_LOG" 2>&1 &
  fi
}
