//! Shell command policy validator and sandboxed executor.
//!
//! Submodules:
//! - [`scanner`]: Lexical command scanning and security validation rules.
//! - [`process`]: Cross-platform command spawning, stream capture, and process tree lifecycle management.
//!
//! Coordinates execution via [`BashExecutor`], [`run`], and [`run_async`].

pub mod process;
pub mod scanner;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::time::Duration;

pub use process::{ShellError, ShellOutput};
pub use scanner::{CANARY_DISASTER_FUSE_TOKEN, ShellPolicyError, validate, validate_with_context};

/// Sandboxed bash command executor coordinating policy validation,
/// sandbox resolution, and process tree lifecycle management.
#[derive(Debug, Clone, Copy, Default)]
pub struct BashExecutor;

impl BashExecutor {
    /// Create a new bash executor instance.
    pub fn new() -> Self {
        Self
    }

    /// Executes a shell command asynchronously with layered safety:
    /// 1. Resolve `cwd` through the sandbox containment layers.
    /// 2. Validate against security scanner policy using real context.
    /// 3. Execute with process group isolation and timeout supervision.
    pub async fn run_async(
        &self,
        cmd: &str,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<ShellOutput, ShellError> {
        run_async(cmd, cwd, timeout).await
    }

    /// Executes a shell command synchronously by driving [`run_async`] on a current-thread runtime.
    pub fn run(&self, cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellOutput, ShellError> {
        run(cmd, cwd, timeout)
    }
}

/// Executes a shell command asynchronously with layered safety:
///
/// 1. `cwd` resolved through the sandbox layers (canonicalized real path) so
///    the policy sees the actual workspace root.
/// 2. `validate_with_context` — policy validation against the real cwd and the
///    real user home (typed refusal; nothing is ever spawned on refusal).
/// 3. Spawn `bash -c <cmd>` with `process_group(0)`.
pub async fn run_async(
    cmd: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<ShellOutput, ShellError> {
    let cwd = crate::tools::sandbox::resolve_workspace_root(cwd)?;
    let home_str = scanner::resolve_home().unwrap_or_else(|| scanner::DEFAULT_HOME.to_string());
    let home = std::path::Path::new(&home_str);
    validate_with_context(cmd, &cwd, home)?;
    process::execute(cmd, &cwd, timeout).await
}

/// Executes a shell command synchronously by driving `run_async` on a current-thread runtime.
pub fn run(cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellOutput, ShellError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async(cmd, cwd, timeout))
}
