//! Native sandboxed Git tools: `git_status`, `git_diff`, `git_log`, `git_commit`.
//!
//! All path arguments flow through the [`SandboxPolicy`] before reaching git.
//! Git is invoked directly via `tokio::process::Command` — never through a shell.

use std::path::Path;

use super::sandbox::{SandboxError, SandboxPolicy};

/// Error type for git tool operations.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("{0}")]
    Sandbox(#[from] SandboxError),
    #[error("git: {0}")]
    Execution(String),
    #[error("{0}")]
    InvalidArgs(String),
}

fn root(policy: &SandboxPolicy) -> &Path {
    policy
        .write_roots()
        .first()
        .map(|p| p.as_path())
        .unwrap_or_else(|| Path::new("."))
}

fn resolve_read(policy: &SandboxPolicy, path: &str) -> Result<std::path::PathBuf, GitError> {
    let resolved = policy.resolve(path)?;
    if !resolved.permits_read() {
        return Err(GitError::Sandbox(SandboxError::PathEscape(format!(
            "PathEscapeError: Access denied. Path '{path}' does not permit this operation"
        ))));
    }
    Ok(resolved.path)
}

fn resolve_write(policy: &SandboxPolicy, path: &str) -> Result<std::path::PathBuf, GitError> {
    let resolved = policy.resolve(path)?;
    if !resolved.permits_write() {
        return Err(GitError::Sandbox(SandboxError::PathEscape(format!(
            "PathEscapeError: Access denied. Path '{path}' does not permit this operation"
        ))));
    }
    Ok(resolved.path)
}

/// Returns a path relative to the root for use as a git argument.
fn rel_to_root(root: &Path, resolved: &Path) -> String {
    resolved
        .strip_prefix(root)
        .unwrap_or(resolved)
        .to_string_lossy()
        .into_owned()
}

/// Run `git status -sb` (optionally scoped to a path).
pub async fn git_status(policy: &SandboxPolicy, path: Option<&str>) -> Result<String, GitError> {
    let root = root(policy);
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(root);
    cmd.arg("status").arg("-sb");
    if let Some(p) = path {
        let resolved = resolve_read(policy, p)?;
        cmd.arg("--").arg(rel_to_root(root, &resolved));
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| GitError::Execution(e.to_string()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let mut text = String::new();
    if !out.status.success() {
        use std::fmt::Write;
        let _ = writeln!(text, "(exit code {})", out.status.code().unwrap_or(-1));
    }
    if !stdout.is_empty() {
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("stderr:\n");
        text.push_str(&stderr);
    }
    if text.is_empty() {
        text = "(no changes; working tree clean)".to_string();
    }
    Ok(text)
}

/// Run `git diff` with optional `--staged`, `--stat`, and path filter.
pub async fn git_diff(
    policy: &SandboxPolicy,
    staged: bool,
    stat: bool,
    path: Option<&str>,
) -> Result<String, GitError> {
    let root = root(policy);
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(root);
    cmd.arg("diff");
    if staged {
        cmd.arg("--staged");
    }
    if stat {
        cmd.arg("--stat");
    }
    if let Some(p) = path {
        let resolved = resolve_read(policy, p)?;
        cmd.arg("--").arg(rel_to_root(root, &resolved));
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| GitError::Execution(e.to_string()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let mut text = String::new();
    if !out.status.success() {
        use std::fmt::Write;
        let _ = writeln!(text, "(exit code {})", out.status.code().unwrap_or(-1));
    }
    if !stdout.is_empty() {
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("stderr:\n");
        text.push_str(&stderr);
    }
    if text.is_empty() {
        text = "(no diff)".to_string();
    }
    Ok(text)
}

/// Run `git log -n <limit> --oneline` (optionally scoped to a path).
/// `limit` is clamped to `1..=100` (default `10`).
pub async fn git_log(
    policy: &SandboxPolicy,
    limit: Option<u32>,
    path: Option<&str>,
) -> Result<String, GitError> {
    let root = root(policy);
    let limit = limit.unwrap_or(10).clamp(1, 100);
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(root);
    cmd.arg("log").arg(format!("-n{limit}")).arg("--oneline");
    if let Some(p) = path {
        let resolved = resolve_read(policy, p)?;
        cmd.arg("--").arg(rel_to_root(root, &resolved));
    }
    let out = cmd
        .output()
        .await
        .map_err(|e| GitError::Execution(e.to_string()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let mut text = String::new();
    if !out.status.success() {
        use std::fmt::Write;
        let _ = writeln!(text, "(exit code {})", out.status.code().unwrap_or(-1));
    }
    if !stdout.is_empty() {
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("stderr:\n");
        text.push_str(&stderr);
    }
    if text.is_empty() {
        text = "(no commits yet)".to_string();
    }
    Ok(text)
}

/// Stage paths (validated for write access) and commit with a message.
///
/// Rejects an empty `message`. If `paths` is non-empty, each path is validated
/// via `SandboxPolicy` (write access required) before `git add` is run.
pub async fn git_commit(
    policy: &SandboxPolicy,
    message: &str,
    paths: Option<&[String]>,
) -> Result<String, GitError> {
    if message.trim().is_empty() {
        return Err(GitError::InvalidArgs(
            "commit message must not be empty".into(),
        ));
    }
    let root = root(policy);
    let has_paths = paths.is_some_and(|ps| !ps.is_empty());

    if has_paths {
        // Paths provided: stage only those, then commit ONLY those paths
        // using `--only -- <paths>` so pre-staged index files outside `paths`
        // can NEVER sneak into the commit.
        let ps = paths.unwrap();
        let rel_paths: Vec<String> = ps
            .iter()
            .map(|p| -> Result<String, GitError> {
                let resolved = resolve_write(policy, p)?;
                Ok(rel_to_root(root, &resolved))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut add_cmd = tokio::process::Command::new("git");
        add_cmd.current_dir(root);
        add_cmd.arg("add").arg("--");
        for p in &rel_paths {
            add_cmd.arg(p);
        }
        let out = add_cmd
            .output()
            .await
            .map_err(|e| GitError::Execution(e.to_string()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            return Err(GitError::Execution(format!("git add failed: {stderr}")));
        }

        let mut commit_cmd = tokio::process::Command::new("git");
        commit_cmd.current_dir(root);
        commit_cmd
            .arg("commit")
            .arg("-m")
            .arg(message)
            .arg("--only")
            .arg("--");
        for p in &rel_paths {
            commit_cmd.arg(p);
        }
        let out = commit_cmd
            .output()
            .await
            .map_err(|e| GitError::Execution(e.to_string()))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let mut text = String::new();
        if !out.status.success() {
            use std::fmt::Write;
            let _ = writeln!(text, "(exit code {})", out.status.code().unwrap_or(-1));
        }
        if !stdout.is_empty() {
            text.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str("stderr:\n");
            text.push_str(&stderr);
        }
        if text.is_empty() {
            text = "(nothing to commit)".to_string();
        }
        return Ok(text);
    }

    // No paths provided: validate ALL staged files against the sandbox
    // write-policy before committing. Reject if nothing is staged or if
    // any staged path fails policy validation.
    let mut diff_cmd = tokio::process::Command::new("git");
    diff_cmd.current_dir(root);
    diff_cmd.arg("diff").arg("--cached").arg("--name-only");
    let out = diff_cmd
        .output()
        .await
        .map_err(|e| GitError::Execution(e.to_string()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let staged: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if staged.is_empty() {
        return Err(GitError::InvalidArgs("nothing staged to commit".into()));
    }

    for p in &staged {
        let _ = resolve_write(policy, p)?;
    }

    let mut commit_cmd = tokio::process::Command::new("git");
    commit_cmd.current_dir(root);
    commit_cmd.arg("commit").arg("-m").arg(message);
    let out = commit_cmd
        .output()
        .await
        .map_err(|e| GitError::Execution(e.to_string()))?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let mut text = String::new();
    if !out.status.success() {
        use std::fmt::Write;
        let _ = writeln!(text, "(exit code {})", out.status.code().unwrap_or(-1));
    }
    if !stdout.is_empty() {
        text.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("stderr:\n");
        text.push_str(&stderr);
    }
    if text.is_empty() {
        text = "(nothing to commit)".to_string();
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_git_repo() -> (PathBuf, SandboxPolicy) {
        let dir = std::env::temp_dir().join(format!(
            "castor_git_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // git init + config
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .expect("git should be available");
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test User"]);

        let policy = SandboxPolicy::workspace_only(&dir);
        (dir, policy)
    }

    #[tokio::test]
    async fn git_status_clean_repo() {
        let (_dir, policy) = temp_git_repo();
        let out = git_status(&policy, None).await.unwrap();
        assert!(out.contains("main"), "got: {out}");
        let _ = fs::remove_dir_all(_dir);
    }

    #[tokio::test]
    async fn git_commit_and_log() {
        let (dir, policy) = temp_git_repo();
        fs::write(dir.join("hello.txt"), "hello\n").unwrap();

        // Commit
        let out = git_commit(&policy, "Initial commit", Some(&["hello.txt".to_string()]))
            .await
            .unwrap();
        assert!(
            out.contains("1 file changed") || out.contains("commit"),
            "got: {out}"
        );

        // Log should show the commit
        let log = git_log(&policy, Some(5), None).await.unwrap();
        assert!(log.contains("Initial commit"), "got: {log}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn git_diff_detects_changes() {
        let (dir, policy) = temp_git_repo();
        fs::write(dir.join("a.txt"), "line1\n").unwrap();
        let _ = git_commit(&policy, "Initial", Some(&["a.txt".to_string()])).await;

        // Modify the file
        fs::write(dir.join("a.txt"), "line1\nline2\n").unwrap();
        let diff = git_diff(&policy, false, false, None).await.unwrap();
        assert!(diff.contains("line2"), "got: {diff}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn git_rejects_escape_path() {
        let (dir, policy) = temp_git_repo();
        let _ = fs::remove_dir_all(&dir);

        // `../escape` should be blocked by the sandbox
        let err = git_status(&policy, Some("../escape")).await.unwrap_err();
        match err {
            GitError::Sandbox(SandboxError::PathEscape(msg)) => {
                assert!(msg.contains("escape"), "got: {msg}");
            }
            other => panic!("expected PathEscape, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn git_commit_rejects_empty_message() {
        let (dir, policy) = temp_git_repo();
        let _ = fs::remove_dir_all(&dir);
        let err = git_commit(&policy, "", None).await.unwrap_err();
        assert!(matches!(err, GitError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn git_commit_does_not_include_pre_staged_files_outside_paths() {
        let (dir, policy) = temp_git_repo();
        fs::write(dir.join("target.txt"), "target\n").unwrap();
        fs::write(dir.join("sneaky.txt"), "sneaky\n").unwrap();

        // Pre-stage BOTH files in the index directly (simulating a
        // pre-existing index state outside of `git_commit`'s control).
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .expect("git should be available");
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["add", "--", "target.txt", "sneaky.txt"]);

        // Commit ONLY target.txt. sneaky.txt is pre-staged but not in paths.
        let out = git_commit(&policy, "partial commit", Some(&["target.txt".to_string()]))
            .await
            .unwrap();
        assert!(
            out.contains("1 file changed") || out.contains("commit"),
            "got: {out}"
        );

        // Verify sneaky.txt was NOT committed: it must still be untracked/staged.
        let log = git_log(&policy, Some(5), None).await.unwrap();
        assert!(log.contains("partial commit"), "got: {log}");

        // The HEAD commit must contain only target.txt.
        let out = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["ls-tree", "--name-only", "HEAD"])
            .output()
            .expect("git should be available");
        let tree = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            tree.contains("target.txt"),
            "target.txt should be in HEAD: {tree}"
        );
        assert!(
            !tree.contains("sneaky.txt"),
            "sneaky.txt (pre-staged, outside paths) must NOT be in HEAD: {tree}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
