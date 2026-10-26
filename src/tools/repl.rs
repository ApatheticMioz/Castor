//! In-session Python REPL scratchpad (RFC Issue #17, Part 2).
//!
//! `PythonRepl` owns a single long-lived `python3 -u -c <DRIVER>` child
//! process whose stdin/stdout are JSON-line channels. Each call to
//! [`PythonRepl::eval`] sends `{"code": "..."}` and reads one response
//! line `{"ok": bool, "output": "..."}`.
//!
//! The inline `<DRIVER>` script (see [`DRIVER_SRC`]) keeps a persistent
//! `ns = {"__name__": "__main__"}` dict across calls, uses `ast.parse`
//! to split a trailing expression from the preceding statements, and
//! captures `stdout` / `stderr` / tracebacks via `io.StringIO` so the
//! REPL's protocol channel is never polluted.

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// The inline Python driver script.
///
/// Contract:
/// - On startup, prints `{"ready": true}` to stdout (flush).
/// - Reads one JSON object per line from stdin: `{"code": "..."}`.
/// - For each request, prints exactly one JSON object to stdout (flush):
///   `{"ok": true/false, "output": "..."}`.
///
/// The script is designed to be robust against broken Windows Store
/// `python3.exe` stubs: if the interpreter fails to start or does not
/// emit the ready line within the 5-second timeout in `spawn`, the
/// candidate is skipped and the next one is tried.
const DRIVER_SRC: &str = r#"
import ast, io, json, os, sys, traceback

STDIN_FD = 0
STDOUT_FD = 1

def _read_line():
    """Read one line from the raw stdin fd (bypasses TextIOWrapper buffering)."""
    buf = b""
    while b"\n" not in buf:
        chunk = os.read(STDIN_FD, 4096)
        if not chunk:
            return None
        buf += chunk
    return buf.decode("utf-8", errors="replace").rstrip("\n")

def _write_line(s):
    os.write(STDOUT_FD, (s + "\n").encode("utf-8"))

ns = {"__name__": "__main__"}

def run(code):
    out = io.StringIO()
    err = io.StringIO()
    old_out, old_err = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = out, err
    ok = True
    parts = []
    try:
        tree = ast.parse(code)
        body = tree.body
        if body and isinstance(body[-1], ast.Expr):
            prefix = body[:-1]
            if prefix:
                exec(compile(ast.Module(body=prefix, type_ignores=[]), "<repl>", "exec"), ns)
            val = eval(compile(ast.Expression(body=body[-1].value), "<repl>", "eval"), ns)
            if val is not None:
                parts.append(repr(val))
        else:
            exec(compile(tree, "<repl>", "exec"), ns)
    except BaseException:
        ok = False
        parts.append(traceback.format_exc())
    finally:
        sys.stdout, sys.stderr = old_out, old_err
    captured = out.getvalue()
    err_out = err.getvalue()
    if captured:
        parts.append(captured)
    if err_out:
        parts.append(err_out)
    output = "\n".join(parts) if parts else ""
    _write_line(json.dumps({"ok": ok, "output": output}))

_write_line(json.dumps({"ready": True}))

while True:
    line = _read_line()
    if line is None:
        break
    line = line.strip()
    if not line:
        continue
    try:
        req = json.loads(line)
    except json.JSONDecodeError:
        _write_line(json.dumps({"ok": False, "output": "invalid JSON request"}))
        continue
    run(req.get("code", ""))
"#;

/// A single long-lived Python child process speaking the JSON-line protocol.
struct ReplProcess {
    child: Child,
    stdin: ChildStdin,
    stdout_lines: Lines<BufReader<ChildStdout>>,
}

impl ReplProcess {
    /// Spawn a Python interpreter with the driver script, trying candidate
    /// binary names in order. Returns the first process that emits a valid
    /// `{"ready": true}` line within 5 seconds.
    ///
    /// Candidate order:
    /// - Windows: `python` first, then `python3` (avoids broken Store stubs).
    /// - Unix: `python3` first, then `python`.
    async fn spawn(workspace: &std::path::Path) -> Result<Self, String> {
        let candidates: &[&str] = if cfg!(windows) {
            &["python", "python3"]
        } else {
            &["python3", "python"]
        };

        let mut last_err = String::new();

        for &bin in candidates {
            // `-u` disables stdio buffering so our JSON lines flush immediately.
            let mut cmd = tokio::process::Command::new(bin);
            cmd.args(["-u", "-c", DRIVER_SRC])
                .current_dir(workspace)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    last_err = format!("{bin}: {e}");
                    continue;
                }
            };

            let stdin = child.stdin.take().ok_or_else(|| {
                let _ = child.start_kill();
                format!("{bin}: failed to take stdin")
            })?;
            let stdout = child.stdout.take().ok_or_else(|| {
                let _ = child.start_kill();
                format!("{bin}: failed to take stdout")
            })?;

            // Wait for the ready line within 5 seconds.
            let mut lines = BufReader::new(stdout).lines();
            let ready = tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await;

            match ready {
                Ok(Ok(Some(line))) if line.trim() == "{\"ready\": true}" => {
                    return Ok(Self {
                        child,
                        stdin,
                        stdout_lines: lines,
                    });
                }
                _ => {
                    // Kill the failed candidate and try the next one.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    last_err = format!("{bin}: no valid ready line within 5s");
                }
            }
        }

        Err(format!(
            "no working Python interpreter found (last: {last_err})"
        ))
    }

    /// Send a code line and read the single JSON response line.
    ///
    /// Returns the `output` field on success, or an error string on
    /// timeout (30s) / EOF / protocol violation.
    async fn eval_once(&mut self, code: &str) -> Result<String, String> {
        let payload = serde_json::json!({"code": code}).to_string();
        // The Python driver reads line-by-line (waits for \n).
        let payload = format!("{payload}\n");
        // On a write/flush I/O error the child is gone (pipe broken); kill
        // it explicitly before the error propagates so no zombie lingers.
        if let Err(e) = self.stdin.write_all(payload.as_bytes()).await {
            let _ = self.child.start_kill();
            return Err(format!("write to REPL stdin: {e}"));
        }
        if let Err(e) = self.stdin.flush().await {
            let _ = self.child.start_kill();
            return Err(format!("flush REPL stdin: {e}"));
        }

        let line = tokio::time::timeout(Duration::from_secs(30), self.stdout_lines.next_line())
            .await
            // Timeout: the child is stuck (e.g. a long-running eval); kill it
            // explicitly before dropping so the process does not outlive us.
            .map_err(|_| {
                let _ = self.child.start_kill();
                "REPL eval timed out after 30s".to_string()
            })?;

        let line = match line {
            Ok(Some(l)) => l,
            Ok(None) => {
                let _ = self.child.start_kill();
                return Err("REPL process closed (EOF)".to_string());
            }
            Err(e) => {
                let _ = self.child.start_kill();
                return Err(format!("read from REPL stdout: {e}"));
            }
        };

        let v: serde_json::Value = serde_json::from_str(&line)
            .map_err(|e| format!("invalid JSON from REPL: {e}: {line}"))?;

        let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
        let output = v
            .get("output")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();

        if ok {
            Ok(output)
        } else {
            // ok=false means the Python code itself raised — that is a
            // valid session response, not a transport error. The caller
            // (the tool layer) decides how to surface it.
            Err(output)
        }
    }
}

/// In-session Python REPL scratchpad tool.
///
/// Holds at most one live [`ReplProcess`] per executor instance. The
/// process is created lazily on the first `eval` call and destroyed on
/// `reset: true` or on any transport failure.
pub struct PythonRepl {
    workspace: PathBuf,
    session: Mutex<Option<ReplProcess>>,
}

impl PythonRepl {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            session: Mutex::new(None),
        }
    }

    /// Evaluate `code` in the persistent REPL session.
    ///
    /// - `reset: true` drops any existing process before evaluating.
    /// - If no process exists, one is spawned (candidates tried in order).
    /// - On success, returns the combined stdout + repr-of-final-expr text.
    /// - On Python-level error (the code itself raised), the traceback is
    ///   returned as the error string; the session survives.
    /// - On transport failure (timeout, EOF, spawn error), the session is
    ///   dropped so the next call starts fresh.
    pub async fn eval(&self, code: &str, reset: bool) -> Result<String, String> {
        let mut guard = self.session.lock().await;

        if reset {
            if let Some(proc) = guard.as_mut() {
                let _ = proc.child.start_kill();
                let _ = proc.child.wait().await;
            }
            *guard = None;
        }

        if guard.is_none() {
            match ReplProcess::spawn(&self.workspace).await {
                Ok(p) => *guard = Some(p),
                Err(e) => return Err(e),
            }
        }

        match guard.as_mut().unwrap().eval_once(code).await {
            Ok(out) => Ok(out),
            Err(msg) => {
                // Distinguish: if the message looks like a Python traceback
                // (contains "Traceback"), the session is still alive — keep
                // it. Otherwise it is a transport error — kill the process.
                if msg.contains("Traceback") || msg.is_empty() {
                    // Session survived; the error is a Python-level one.
                    Err(msg)
                } else {
                    // Transport failure — drop the session for next call.
                    if let Some(proc) = guard.as_mut() {
                        let _ = proc.child.start_kill();
                        let _ = proc.child.wait().await;
                    }
                    *guard = None;
                    Err(msg)
                }
            }
        }
    }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────
//
// These tests require a working Python 3 interpreter on the test host.
// They are gated on the same conditions as the rest of the tools tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "repl_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// (a) Variable persistence: `x = 21` then `x * 2` → `"42"`.
    #[tokio::test]
    async fn repl_persists_variables_across_evals() {
        let ws = tmp_dir();
        let repl = PythonRepl::new(&ws);

        let r1 = repl.eval("x = 21", false).await;
        assert!(r1.is_ok(), "first eval should succeed: {:?}", r1.err());
        // `x = 21` is a bare assignment → no repr output, just empty.
        assert_eq!(r1.as_ref().unwrap(), "");

        let r2 = repl.eval("x * 2", false).await;
        assert!(r2.is_ok(), "second eval should succeed: {:?}", r2.err());
        assert_eq!(r2.as_ref().unwrap(), "42");

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// (b) `reset: true` clears prior variables → `NameError`.
    #[tokio::test]
    async fn repl_reset_clears_namespace() {
        let ws = tmp_dir();
        let repl = PythonRepl::new(&ws);

        let _ = repl.eval("x = 21", false).await.unwrap();

        // Reset and try to access `x` → should be a NameError.
        let r = repl.eval("x", true).await;
        assert!(r.is_err(), "expected NameError after reset, got {:?}", r);
        let err = r.unwrap_err();
        assert!(
            err.contains("NameError") || err.contains("x"),
            "expected NameError in: {err}"
        );

        let _ = std::fs::remove_dir_all(&ws);
    }

    /// (c) A syntax error returns a traceback without killing the session;
    /// a subsequent valid call succeeds.
    #[tokio::test]
    async fn repl_syntax_error_does_not_kill_session() {
        let ws = tmp_dir();
        let repl = PythonRepl::new(&ws);

        // Seed a variable.
        let _ = repl.eval("y = 100", false).await.unwrap();

        // Syntax error.
        let err = repl.eval("def broken(:", false).await;
        assert!(
            err.is_err(),
            "expected error for syntax error, got: {:?}",
            err
        );
        let err_msg = err.unwrap_err();
        assert!(
            err_msg.contains("SyntaxError"),
            "expected SyntaxError in: {err_msg}"
        );

        // Session should still be alive — `y` is still set.
        let r = repl.eval("y", false).await;
        assert!(
            r.is_ok(),
            "session should survive syntax error: {:?}",
            r.err()
        );
        assert_eq!(r.as_ref().unwrap(), "100");

        let _ = std::fs::remove_dir_all(&ws);
    }
}
