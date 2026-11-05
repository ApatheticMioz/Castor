//! Filesystem tools on the runner's `ToolExecutor` surface.
//!
//! Tools: `read_file`, `write_file`, `edit_file`, `list_dir`, `search_code`.
//! All paths flow through the sandbox layers in [`super::sandbox`].

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

use crate::runner::{ToolError, ToolExecutor, ToolOutcome};

use super::sandbox::{self, AccessClass, SandboxError, SandboxPolicy};

use base64::prelude::*;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("{0}")]
    Sandbox(#[from] SandboxError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    InvalidArgs(String),
    #[error("{0}")]
    EditError(String),
}

/// Standard RFC 4648 base64 encoder for image data URLs.
pub fn base64_encode(data: &[u8]) -> String {
    BASE64_STANDARD.encode(data)
}

pub struct FsExecutor {
    policy: SandboxPolicy,
}

impl FsExecutor {
    /// Construct with the tight single-root policy (workspace only). This is
    /// what the eval replay runner uses, where only the workspace may be
    /// touched.
    pub fn new(root: &Path) -> Result<Self, FsError> {
        let policy = SandboxPolicy::workspace_only(root);
        Ok(Self { policy })
    }

    /// Construct with an explicit [`SandboxPolicy`] (e.g. the production policy
    /// built by [`crate::tools::CompositeExecutor::with_config`], which adds the
    /// read-only state dir and, on Linux, the system read roots).
    pub fn with_policy(policy: SandboxPolicy) -> Self {
        Self { policy }
    }

    /// Access the underlying [`SandboxPolicy`] (used by the filesystem and
    /// shell executors to validate paths).
    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// The primary write root (the workspace), used as the cwd and as the base
    /// for relative-path resolution / relative display.
    fn root(&self) -> &Path {
        self.policy
            .write_roots()
            .first()
            .map(|p| p.as_path())
            .unwrap_or_else(|| Path::new("."))
    }

    /// Resolve `input` for a **read** operation (RW ∪ RO roots allowed).
    fn resolve_read(&self, input: &str) -> Result<PathBuf, FsError> {
        self.resolve_with(input, AccessClass::ReadOnly)
    }

    /// Resolve `input` for a **write** operation (RW roots only).
    fn resolve_write(&self, input: &str) -> Result<PathBuf, FsError> {
        self.resolve_with(input, AccessClass::ReadWrite)
    }

    fn resolve_with(&self, input: &str, required: AccessClass) -> Result<PathBuf, FsError> {
        let resolved = self.policy.resolve(input)?;
        let ok = match required {
            AccessClass::ReadWrite => resolved.class == AccessClass::ReadWrite,
            AccessClass::ReadOnly => resolved.permits_read(),
            AccessClass::Deny => false,
        };
        if !ok {
            return Err(SandboxError::PathEscape(format!(
                "PathEscapeError: Access denied. Path '{}' does not permit this operation",
                input
            ))
            .into());
        }
        Ok(resolved.path)
    }

    fn read_file(
        &self,
        path: &str,
        start_line: usize,
        end_line: Option<usize>,
    ) -> Result<String, FsError> {
        let resolved = self.resolve_read(path)?;
        if !resolved.exists() {
            return Err(FsError::InvalidArgs(format!("File not found: {path}")));
        }
        // Guard against non-regular files (FIFOs, sockets, device nodes): opening
        // one for reading would block a tokio worker thread on an unbounded
        // read. `symlink_metadata` inspects the path itself, so a symlink that
        // points at a regular file still passes (it is dereferenced by the reads
        // below) while a symlink to a FIFO is refused here instead of hanging.
        let meta = resolved.symlink_metadata()?;
        if !meta.file_type().is_file() {
            return Err(FsError::InvalidArgs(format!(
                "Cannot read non-regular file (FIFO, socket, or device): {path}"
            )));
        }
        let ext = resolved.extension().and_then(|e| e.to_str()).unwrap_or("");
        let is_pdf = ext.eq_ignore_ascii_case("pdf");
        let is_image = matches!(
            ext.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
        );
        let content = if is_pdf {
            let bytes = fs::read(&resolved)?;
            let pdf_res = super::web::extract_pdf_text(&bytes, None)
                .map_err(|e| FsError::InvalidArgs(format!("PDF parse error: {e}")))?;
            pdf_res.text
        } else if is_image {
            let bytes = fs::read(&resolved)?;
            if bytes.is_empty() {
                return Err(FsError::InvalidArgs(format!(
                    "Image file '{}' is empty (0 bytes)",
                    resolved.display()
                )));
            }
            const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err(FsError::InvalidArgs(format!(
                    "Image file '{}' exceeds maximum allowed size ({} bytes > 20 MB ceiling)",
                    resolved.display(),
                    bytes.len()
                )));
            }
            let mime = match ext.to_ascii_lowercase().as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "webp" => "image/webp",
                "gif" => "image/gif",
                "bmp" => "image/bmp",
                _ => "image/png",
            };
            let b64 = base64_encode(&bytes);
            return Ok(format!(
                "[IMAGE_ATTACHMENT:data:{mime};base64,{b64}:path:{}]\nSuccessfully loaded image file '{}' ({} bytes) for visual inspection.",
                resolved.display(),
                resolved.display(),
                bytes.len()
            ));
        } else {
            sandbox::check_binary(&resolved)?;
            fs::read_to_string(&resolved)?
        };
        let lines: Vec<&str> = content.split('\n').collect();
        let total = lines.len();
        let (raw_start, raw_end) = match end_line {
            Some(e) => {
                let s = start_line.max(1);
                let e = e.max(1);
                (s.min(e), s.max(e))
            }
            None => {
                let s = start_line.max(1);
                (s, s.saturating_add(800).saturating_sub(1))
            }
        };
        let start = raw_start.saturating_sub(1).min(total);
        let end = raw_end.min(total).max(start);
        let slice = &lines[start..end];
        let numbered: Vec<String> = slice
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let content = truncate_line(line, MAX_READ_LINE_CHARS);
                format!("{}: {}", start + i + 1, content)
            })
            .collect();
        let showing_start = if slice.is_empty() { start } else { start + 1 };
        Ok(format!(
            "{} (total: {} lines, showing: {}-{})\n{}",
            resolved.display(),
            total,
            showing_start,
            end,
            numbered.join("\n")
        ))
    }

    fn write_file(&self, path: &str, content: &str, overwrite: bool) -> Result<String, FsError> {
        let resolved = self.resolve_write(path)?;
        if resolved == self.root() {
            return Err(FsError::InvalidArgs(format!(
                "Target path '{path}' resolves to the workspace root directory, not a file"
            )));
        }
        if resolved.exists() {
            let meta = fs::metadata(&resolved)?;
            if meta.is_dir() {
                return Err(FsError::InvalidArgs(format!(
                    "Target path '{path}' is an existing directory, cannot overwrite as file"
                )));
            }
            if !overwrite {
                return Err(FsError::InvalidArgs(format!(
                    "File already exists and overwrite is false: {path}"
                )));
            }
        }
        self.atomic_write(&resolved, content)?;
        Ok(format!(
            "Wrote {} bytes to {}",
            content.len(),
            resolved.display()
        ))
    }

    fn render_diff_snippet(target: &str, replacement: &str) -> String {
        use similar::{ChangeTag, TextDiff};
        use std::fmt::Write;

        let diff = TextDiff::from_lines(target, replacement);
        let mut out = String::from("\n```diff\n");
        for (count, change) in diff.iter_all_changes().enumerate() {
            if count >= 12 {
                out.push_str("  ...\n");
                break;
            }
            let sign = match change.tag() {
                ChangeTag::Delete => "-",
                ChangeTag::Insert => "+",
                ChangeTag::Equal => " ",
            };
            let line = change.value().trim_end_matches(['\r', '\n']);
            let _ = writeln!(out, "{sign} {line}");
        }
        out.push_str("```");
        out
    }

    fn edit_file(
        &self,
        path: &str,
        target: &str,
        replacement: &str,
        replace_all: bool,
    ) -> Result<String, FsError> {
        if target.is_empty() {
            return Err(FsError::EditError("target_content cannot be empty".into()));
        }
        let resolved = self.resolve_write(path)?;
        if !resolved.exists() {
            return Err(FsError::InvalidArgs(format!(
                "File not found for edit: {path}"
            )));
        }
        let original = fs::read_to_string(&resolved)?;

        let crlf_target = target.replace("\r\n", "\n").replace('\n', "\r\n");
        let lf_target = target.replace("\r\n", "\n");

        let not_found_msg = |path: &str| {
            format!(
                "Target content not found in file: {path}. \
                 Re-read the file with read_file around the target lines to refresh content before retrying."
            )
        };

        let (effective_target, local_style) = if original.contains(target) {
            (target.to_string(), line_ending_style(target))
        } else if crlf_target != target && original.contains(&crlf_target) {
            (crlf_target, Some("crlf"))
        } else if lf_target != target && original.contains(&lf_target) {
            (lf_target, Some("lf"))
        } else {
            return Err(FsError::EditError(not_found_msg(path)));
        };

        let count = original.matches(&effective_target).count();
        if count == 0 {
            return Err(FsError::EditError(not_found_msg(path)));
        }
        if count > 1 && !replace_all {
            return Err(FsError::EditError(format!(
                "AmbiguousTargetError: target_content found {count} times in file: {path}. \
                 Provide a longer unique target or pass replace_all: true. No write performed."
            )));
        }

        let effective_replacement = normalize_line_endings(replacement, local_style);
        let updated = if replace_all {
            original.replace(&effective_target, &effective_replacement)
        } else {
            original.replacen(&effective_target, &effective_replacement, 1)
        };

        // AST gate: when the `ast` feature is enabled, parse the fully-rewritten
        // content in memory before committing it to disk. If the resolved
        // grammar flags an ERROR/UNDEFINED node, the write is refused and the
        // file is left byte-identical. Without the feature, `edit_file` performs
        // the plain exact-substring replacement only (no compile, no AST call).
        #[cfg(feature = "ast")]
        if let Err(e) = super::ast::validate_file_syntax(&resolved, &updated) {
            let detail = match e {
                super::ast::AstError::SyntaxFailure(detail) => detail,
                other => other.to_string(),
            };
            return Err(FsError::EditError(format!(
                "SyntaxValidationError: replacement introduces syntax errors in {path}. No write performed. {detail}"
            )));
        }

        self.atomic_write(&resolved, &updated)?;
        let diff_snippet = Self::render_diff_snippet(&effective_target, &effective_replacement);
        Ok(format!(
            "Replaced {} occurrence(s) in {}:{diff_snippet}",
            if replace_all { count } else { 1 },
            resolved.display()
        ))
    }

    fn list_dir(&self, path: &str, max_depth: usize) -> Result<String, FsError> {
        let resolved = self.resolve_read(path)?;
        if !resolved.exists() {
            return Err(FsError::InvalidArgs(format!("Directory not found: {path}")));
        }
        let meta = fs::metadata(&resolved)?;
        if !meta.is_dir() {
            return Err(FsError::InvalidArgs(format!(
                "Path is not a directory: {path}"
            )));
        }

        let mut items = Vec::new();
        let mut truncated = false;
        let mut partial = false;
        let max_items = 100;

        let walker = walkdir::WalkDir::new(&resolved)
            .min_depth(1)
            .max_depth(max_depth)
            .sort_by(|a, b| a.file_name().cmp(b.file_name()))
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                !IGNORED_DIRS.contains(&name.as_ref())
            });

        for entry in walker {
            match entry {
                Ok(e) => {
                    if items.len() >= max_items {
                        truncated = true;
                        break;
                    }
                    let rel = e
                        .path()
                        .strip_prefix(&resolved)
                        .unwrap_or_else(|_| e.path());
                    let rel_str = rel.to_string_lossy().to_string();
                    let kind = if e.file_type().is_dir() {
                        "dir"
                    } else {
                        "file"
                    };
                    items.push((kind.to_string(), rel_str));
                }
                Err(_) => {
                    partial = true;
                }
            }
        }

        use std::fmt::Write;
        let mut out = format!("{} ({} items):\n", resolved.display(), items.len());
        for (kind, rel) in &items {
            let _ = writeln!(out, "  [{kind}] {rel}");
        }
        if truncated {
            out.push_str("  ... [list truncated: maximum 100 items reached; specify deeper path or lower max_depth]\n");
        }
        if partial {
            out.push_str("  [partial: some directories were unreadable]\n");
        }
        Ok(out)
    }

    fn search_code(&self, query: &str, path: &str, max_results: usize) -> Result<String, FsError> {
        let resolved = self.resolve_read(path)?;
        // When the caller passes a single file, do NOT widen the search to its
        // parent directory: `path` names exactly what should be searched, so
        // matches are constrained to that one file (sibling files are excluded).
        if resolved.is_file() {
            if self.is_inside_git_repo(&resolved)
                && !self.is_git_ignored(&resolved)
            {
                return self.git_grep_file(query, &resolved, max_results);
            }
            return self.search_single_file(query, &resolved, max_results);
        }
        let search_dir = resolved;

        if self.is_inside_git_repo(&search_dir) {
            // `git grep` only sees tracked files plus explicitly `--untracked` ones.
            // If the search root lives inside a gitignored directory (e.g. a temp
            // workspace nested under `.scratch/`), git would silently return no
            // results even though the files exist on disk. Fall back to the
            // filesystem walk, which reads them directly.
            if !self.is_git_ignored(&search_dir) {
                return self.git_grep(query, &search_dir, max_results);
            }
        }
        self.fallback_search(query, &search_dir, max_results)
    }

    /// Search a single file with `git grep` by passing its workspace-relative
    /// path as the pathspec, so only that file is searched.
    fn git_grep_file(
        &self,
        query: &str,
        file: &Path,
        max_results: usize,
    ) -> Result<String, FsError> {
        let rel = file.strip_prefix(self.root()).unwrap_or(file);
        let rel_str = rel.to_string_lossy().to_string();
        self.git_grep_with_pathspec(query, Some(&rel_str), max_results)
    }

    fn is_git_ignored(&self, dir: &Path) -> bool {
        let abs = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let output = match std::process::Command::new("git")
            .args(["check-ignore", "-q", "--", abs.to_str().unwrap_or_default()])
            .current_dir(self.root())
            .output()
        {
            Ok(o) => o,
            Err(_) => return false,
        };
        output.status.success()
    }

    fn is_inside_git_repo(&self, dir: &Path) -> bool {
        let mut d = dir.to_path_buf();
        loop {
            if d.join(".git").exists() {
                return true;
            }
            match d.parent() {
                Some(p) if p != d => d = p.to_path_buf(),
                _ => return false,
            }
        }
    }

    fn git_grep(
        &self,
        query: &str,
        search_dir: &Path,
        max_results: usize,
    ) -> Result<String, FsError> {
        // Constrain the search to `search_dir` by passing its workspace-relative
        // path as the `git grep` pathspec.
        let rel = search_dir.strip_prefix(self.root()).unwrap_or(search_dir);
        let rel_str = rel.to_string_lossy().to_string();
        let pathspec = if rel_str.is_empty() || rel_str == "." {
            None
        } else {
            Some(rel_str.as_str())
        };
        self.git_grep_with_pathspec(query, pathspec, max_results)
    }

    /// Run `git grep` with an explicit pathspec (or `None` to search the whole
    /// worktree). Shared by the directory-based [`Self::git_grep`] and the
    /// single-file [`Self::git_grep_file`] entry points.
    fn git_grep_with_pathspec(
        &self,
        query: &str,
        pathspec: Option<&str>,
        max_results: usize,
    ) -> Result<String, FsError> {
        let mut args = vec!["grep", "-n", "-I", "-F", "--untracked", "-e", query];
        if let Some(ps) = pathspec {
            args.push("--");
            args.push(ps);
        }

        let output = std::process::Command::new("git")
            .args(&args)
            .current_dir(self.root())
            .output()?;

        if output.status.code() == Some(1) {
            return Ok("No matches found.".into());
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(FsError::InvalidArgs(format!("git grep failed: {stderr}")));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut lines: Vec<&str> = stdout.lines().collect();
        // Multi-threaded `git grep` can emit matches in a
        // non-deterministic order. Sort the raw lines lexicographically
        // BEFORE truncation so the first `max_results` matches are stable
        // across repeated invocations (warm Turn-1 prefix for KV caching).
        lines.sort();
        let matches: Vec<String> = lines
            .iter()
            .take(max_results)
            .map(|l| truncate_line(l, MAX_SEARCH_LINE_CHARS))
            .collect();

        use std::fmt::Write;
        let mut out = format!("{} match(es) for '{query}':\n", matches.len());
        for m in &matches {
            let _ = writeln!(out, "  {m}");
        }
        if lines.len() > max_results {
            let _ = writeln!(
                out,
                "  ... ({} more matches truncated)",
                lines.len() - max_results
            );
        }
        Ok(out)
    }

    fn fallback_search(
        &self,
        query: &str,
        search_dir: &Path,
        max_results: usize,
    ) -> Result<String, FsError> {
        let mut matches: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        self.walk_search(
            search_dir,
            search_dir,
            query,
            max_results,
            &mut matches,
            &mut skipped,
        )?;

        use std::fmt::Write;
        let mut out = format!("{} match(es) for '{query}':\n", matches.len());
        for m in &matches {
            let _ = writeln!(out, "  {m}");
        }
        if !skipped.is_empty() {
            let _ = writeln!(out, "  ({} binary files skipped)", skipped.len());
        }
        Ok(out)
    }

    /// Search a single file by reading it directly and matching line-by-line.
    /// Used as the non-git fallback when `search_code` is given a file path.
    /// The match prefix is the file's workspace-relative path, so output stays
    /// consistent with the directory-walk format.
    fn search_single_file(
        &self,
        query: &str,
        file: &Path,
        max_results: usize,
    ) -> Result<String, FsError> {
        let rel = file.strip_prefix(self.root()).unwrap_or(file);
        let rel_str = rel.to_string_lossy().to_string();
        let content = fs::read_to_string(file)?;
        let mut matches: Vec<String> = Vec::new();
        for (i, line) in content.lines().enumerate() {
            if matches.len() >= max_results {
                break;
            }
            if line.contains(query) {
                matches.push(format!(
                    "{rel_str}:{}:{}",
                    i + 1,
                    truncate_line(line, MAX_SEARCH_LINE_CHARS)
                ));
            }
        }

        use std::fmt::Write;
        let mut out = format!("{} match(es) for '{query}':\n", matches.len());
        for m in &matches {
            let _ = writeln!(out, "  {m}");
        }
        Ok(out)
    }

    fn walk_search(
        &self,
        base: &Path,
        current: &Path,
        query: &str,
        max_results: usize,
        matches: &mut Vec<String>,
        skipped: &mut Vec<String>,
    ) -> Result<(), FsError> {
        if matches.len() >= max_results {
            return Ok(());
        }
        let dir_iter = match fs::read_dir(current) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        // Collect the OS directory enumeration into a Vec and sort
        // by file_name() so the fallback search order is deterministic
        // (independent of platform directory enumeration order).
        let mut entries: Vec<fs::DirEntry> = dir_iter.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name().to_string_lossy().into_owned());
        for entry in entries {
            if matches.len() >= max_results {
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if IGNORED_DIRS.contains(&name.as_str()) || is_env_file(&name) {
                continue;
            }
            let full = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                self.walk_search(base, &full, query, max_results, matches, skipped)?;
            } else if ft.is_file() {
                let size = entry.metadata()?.len();
                if size < 500_000 {
                    if is_binary_extension(&name) {
                        let rel = full.strip_prefix(base).unwrap_or(&full);
                        skipped.push(format!(
                            "{} (skipped-binary: extension '{}')",
                            rel.to_string_lossy(),
                            name
                        ));
                        continue;
                    }
                    if let Ok(content) = fs::read_to_string(&full) {
                        let rel = full.strip_prefix(base).unwrap_or(&full);
                        let rel_str = rel.to_string_lossy().to_string();
                        for (i, line) in content.lines().enumerate() {
                            if matches.len() >= max_results {
                                break;
                            }
                            if line.contains(query) {
                                matches.push(format!(
                                    "{rel_str}:{}:{}",
                                    i + 1,
                                    truncate_line(line, MAX_SEARCH_LINE_CHARS)
                                ));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn atomic_write(&self, target: &Path, content: &str) -> Result<(), FsError> {
        use std::io::Write;

        let parent = target.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;

        // `tempfile::NamedTempFile` creates its temp file with 0600, and
        // `persist`'s rename keeps those bits — so a rewrite of an existing
        // file would silently downgrade its permissions (e.g. 0644 → 0600).
        // Capture the current permissions up front and restore them after the
        // swap so a rewrite preserves the original file mode.
        let original_perms = target
            .exists()
            .then(|| fs::metadata(target).ok())
            .flatten()
            .map(|m| m.permissions());

        // Crate-first atomic write: `tempfile::NamedTempFile::new_in(parent)`
        // guarantees the temp file lives on the same filesystem as the target
        // (so `persist`'s rename is atomic), `write_all` + `sync_all` force the
        // bytes to disk before the swap, and `persist(target)` atomically
        // renames it into place. Every failure maps to `FsError::Io`.
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(content.as_bytes())?;
        tmp.as_file().sync_all()?;
        if let Err(e) = tmp.persist(target) {
            // `PersistError` carries the underlying `io::Error` (rename/lock
            // failure) plus the un-persisted temp file, which is cleaned up by
            // its `Drop`. Map it to a plain IO error; the existing target is
            // left untouched.
            let io_err: std::io::Error = e.into();
            return Err(FsError::Io(io_err));
        }
        // Restore the pre-existing permissions (if the file already existed)
        // so a rewrite does not downgrade the mode to the tempfile default.
        if let Some(perms) = original_perms {
            fs::set_permissions(target, perms)?;
        }
        Ok(())
    }
}

const IGNORED_DIRS: &[&str] = &[
    ".venv",
    "venv",
    "node_modules",
    ".git",
    "__pycache__",
    "target",
    "dist",
    "build",
    "vendor",
    ".idea",
    ".vscode",
];

fn is_env_file(name: &str) -> bool {
    name == ".env" || name.starts_with(".env.")
}

fn is_binary_extension(name: &str) -> bool {
    let dot = match name.rfind('.') {
        Some(d) if d < name.len() - 1 => d,
        _ => return false,
    };
    let ext = &name[dot + 1..];
    matches!(
        ext.to_lowercase().as_str(),
        "pdf"
            | "png"
            | "jpg"
            | "jpeg"
            | "webp"
            | "gif"
            | "ico"
            | "zip"
            | "gz"
            | "tar"
            | "7z"
            | "bz2"
            | "xz"
            | "wasm"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "bin"
            | "mp3"
            | "mp4"
            | "avi"
            | "mov"
            | "wav"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "sqlite"
            | "db"
    )
}

fn line_ending_style(text: &str) -> Option<&'static str> {
    let has_crlf = text.contains("\r\n");
    let has_bare_lf = text.replace("\r\n", "").contains('\n');
    if has_crlf && has_bare_lf {
        Some("mixed")
    } else if has_crlf {
        Some("crlf")
    } else if has_bare_lf {
        Some("lf")
    } else {
        None
    }
}

fn normalize_line_endings(text: &str, style: Option<&str>) -> String {
    match style {
        Some("crlf") => text.replace("\r\n", "\n").replace('\n', "\r\n"),
        Some("lf") => text.replace("\r\n", "\n"),
        _ => text.to_string(),
    }
}

const MAX_SEARCH_LINE_CHARS: usize = 300;
const MAX_READ_LINE_CHARS: usize = 2000;

fn truncate_line(line: &str, max_chars: usize) -> String {
    if line.chars().count() > max_chars {
        let mut t: String = line.chars().take(max_chars).collect();
        t.push_str(" ... [line truncated]");
        t
    } else {
        line.to_string()
    }
}

#[async_trait]
impl ToolExecutor for FsExecutor {
    async fn execute(&self, name: &str, args_json: &str) -> Result<ToolOutcome, ToolError> {
        let args: Value = serde_json::from_str(args_json).map_err(|e| ToolError::Execute {
            name: name.to_string(),
            message: format!("invalid JSON args: {e}"),
        })?;

        let result = match name {
            "read_file" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let start_line =
                    args.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                let end_line = args
                    .get("end_line")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                self.read_file(path, start_line, end_line)
            }
            "write_file" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let overwrite = args
                    .get("overwrite")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                self.write_file(path, content, overwrite)
            }
            "edit_file" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let target = args
                    .get("target_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let replacement = args
                    .get("replacement_content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let replace_all = args
                    .get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.edit_file(path, target, replacement, replace_all)
            }
            "list_dir" => {
                let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let max_depth =
                    args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(2) as usize;
                self.list_dir(path, max_depth)
            }
            "search_code" => {
                let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let path = args
                    .get("path")
                    .or(args.get("dirPath"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(".");
                let max_results = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50) as usize;
                self.search_code(query, path, max_results)
            }
            _ => Err(FsError::InvalidArgs(format!("unknown tool: {name}"))),
        };

        result
            .map(|text| ToolOutcome { text })
            .map_err(|e| ToolError::Execute {
                name: name.to_string(),
                message: e.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("castor_fs_{}_{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Test root for the Unix-only tests (permission modes, FIFOs).
    ///
    /// `std::env::temp_dir()` can land on a non-Unix filesystem (e.g. a Windows
    /// drvfs 9p mount when `TMPDIR` points at a Windows path) where `chmod` and
    /// `mkfifo` are no-ops / unsupported, which would make these tests fail for
    /// reasons unrelated to the code under test. Probe the base by round-tripping
    /// a `chmod` and fall back to `/tmp` (always a real Unix FS on Linux) when
    /// the mode does not stick.
    #[cfg(unix)]
    fn unix_test_root(name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        fn base_dir() -> PathBuf {
            let probe =
                std::env::temp_dir().join(format!("_castor_perm_probe_{}", std::process::id()));
            if fs::create_dir_all(&probe).is_ok() {
                let f = probe.join("m");
                let ok = fs::write(&f, "x").is_ok()
                    && fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).is_ok()
                    && fs::metadata(&f)
                        .ok()
                        .map(|m| m.permissions().mode() & 0o777 == 0o644)
                        .unwrap_or(false);
                let _ = fs::remove_dir_all(&probe);
                if ok {
                    return std::env::temp_dir();
                }
            }
            PathBuf::from("/tmp")
        }

        let root = base_dir().join(format!("castor_fs_{}_{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn executor(root: &Path) -> FsExecutor {
        FsExecutor::new(root).unwrap()
    }

    #[test]
    fn edit_zero_occurrences_error_untouched() {
        let root = test_root("edit_zero");
        let file = root.join("a.txt");
        fs::write(&file, "hello world\n").unwrap();
        let ex = executor(&root);

        let err = ex
            .edit_file("a.txt", "not present", "x", false)
            .unwrap_err();
        assert!(matches!(err, FsError::EditError(_)));
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello world\n");
    }

    #[test]
    fn edit_two_occurrences_error_untouched() {
        let root = test_root("edit_two");
        let file = root.join("a.txt");
        fs::write(&file, "foo bar foo\n").unwrap();
        let ex = executor(&root);

        let err = ex.edit_file("a.txt", "foo", "baz", false).unwrap_err();
        assert!(matches!(err, FsError::EditError(_)));
        assert_eq!(fs::read_to_string(&file).unwrap(), "foo bar foo\n");
    }

    #[test]
    fn edit_one_occurrence_applied() {
        let root = test_root("edit_one");
        let file = root.join("a.txt");
        fs::write(&file, "hello world\n").unwrap();
        let ex = executor(&root);

        ex.edit_file("a.txt", "world", "there", false).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello there\n");
    }

    #[test]
    fn edit_crlf_preserved() {
        let root = test_root("edit_crlf");
        let file = root.join("a.txt");
        fs::write(&file, "line1\r\nline2\r\nline3\r\n").unwrap();
        let ex = executor(&root);

        ex.edit_file("a.txt", "line2", "LINE2", false).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"line1\r\nLINE2\r\nline3\r\n");
    }

    #[test]
    fn edit_lf_preserved() {
        let root = test_root("edit_lf");
        let file = root.join("a.txt");
        fs::write(&file, "line1\nline2\nline3\n").unwrap();
        let ex = executor(&root);

        ex.edit_file("a.txt", "line2", "LINE2", false).unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"line1\nLINE2\nline3\n");
    }

    #[test]
    fn atomic_write_failure_leaves_target_untouched() {
        let root = test_root("atomic_fail");
        let file = root.join("a.txt");
        fs::write(&file, "original\n").unwrap();
        let ex = executor(&root);

        let err = ex
            .write_file("a.txt/sub.txt", "new content", true)
            .unwrap_err();
        assert!(matches!(err, FsError::Io(_)));
        assert_eq!(fs::read_to_string(&file).unwrap(), "original\n");
    }

    #[test]
    fn binary_read_fail_fast() {
        let root = test_root("binary");
        let bin = root.join("binary.bin");
        fs::write(
            &bin,
            [0x7F, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x00, 0, 0, 0, 0],
        )
        .unwrap();
        let ex = executor(&root);

        let err = ex.read_file("binary.bin", 1, None).unwrap_err();
        assert!(matches!(
            err,
            FsError::Sandbox(SandboxError::BinaryFile { .. })
        ));
    }

    #[test]
    fn read_file_image_returns_attachment() {
        let root = test_root("image_read");
        let png = root.join("img.png");
        fs::write(
            &png,
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0],
        )
        .unwrap();
        let ex = executor(&root);

        let out = ex.read_file("img.png", 1, None).unwrap();
        assert!(out.starts_with("[IMAGE_ATTACHMENT:data:image/png;base64,"));
        assert!(out.contains("Successfully loaded image file"));
    }

    #[test]
    fn read_file_pdf_extracts_text() {
        let root = test_root("pdf_read");
        let pdf = root.join("doc.pdf");
        let pdf_bytes = super::super::web::tests::build_minimal_pdf();
        fs::write(&pdf, &pdf_bytes).unwrap();
        let ex = executor(&root);

        let out = ex.read_file("doc.pdf", 1, None).unwrap();
        assert!(out.contains("Hello"), "PDF text must be extracted: {out}");
    }

    #[test]
    fn out_of_tree_path_refused() {
        let root = test_root("outofree");
        let ex = executor(&root);

        let err = ex.read_file("/etc/passwd", 1, None).unwrap_err();
        assert!(matches!(err, FsError::Sandbox(SandboxError::PathEscape(_))));
    }

    #[test]
    fn base64_rfc4648_test_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn read_file_image_case_insensitive_and_empty_check() {
        let root = test_root("image_edge_cases");
        let ex = executor(&root);

        // Case-insensitivity: .PNG uppercase extension
        let upper_png = root.join("TEST.PNG");
        fs::write(&upper_png, [0x89, b'P', b'N', b'G', 1, 2, 3]).unwrap();
        let out = ex.read_file("TEST.PNG", 1, None).unwrap();
        assert!(out.starts_with("[IMAGE_ATTACHMENT:data:image/png;base64,"));

        // Empty 0-byte image fails fast
        let empty_jpg = root.join("empty.jpg");
        fs::write(&empty_jpg, []).unwrap();
        let err = ex.read_file("empty.jpg", 1, None).unwrap_err();
        assert!(matches!(err, FsError::InvalidArgs(msg) if msg.contains("0 bytes")));

        // Directory with image extension is rejected (non-regular file guard)
        let img_dir = root.join("folder.webp");
        fs::create_dir_all(&img_dir).unwrap();
        let err_dir = ex.read_file("folder.webp", 1, None).unwrap_err();
        assert!(matches!(err_dir, FsError::InvalidArgs(msg) if msg.contains("non-regular file")));
    }

    #[test]
    fn edit_file_returns_unified_diff() {
        let root = test_root("edit_diff");
        let file = root.join("test.rs");
        fs::write(&file, "fn old_logic() -> bool { true }\n").unwrap();
        let ex = executor(&root);

        let out = ex
            .edit_file(
                "test.rs",
                "fn old_logic() -> bool { true }",
                "fn new_logic() -> bool { false }",
                false,
            )
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence(s)"));
        assert!(out.contains("```diff"));
        assert!(out.contains("- fn old_logic() -> bool { true }"));
        assert!(out.contains("+ fn new_logic() -> bool { false }"));
    }

    #[cfg(feature = "ast")]
    #[test]
    fn edit_file_rejects_syntax_error_without_writing() {
        // With the `ast` feature on, an edit whose result does not parse must be
        // refused with a SyntaxValidationError and the file must stay untouched.
        let root = test_root("edit_syntax_gate");
        let file = root.join("lib.rs");
        let original = "fn broken_marker() {}\n";
        fs::write(&file, original).unwrap();
        let ex = executor(&root);

        let err = ex
            .edit_file(
                "lib.rs",
                "fn broken_marker() {}",
                "fn broken_marker() { let x = ;;; }",
                false,
            )
            .unwrap_err();
        assert!(
            matches!(err, FsError::EditError(_)),
            "expected an EditError, got {err:?}"
        );
        assert!(
            err.to_string().contains("SyntaxValidationError"),
            "error must be labelled SyntaxValidationError, got: {err}"
        );
        assert!(
            err.to_string().contains("No write performed"),
            "error must state that no write was performed, got: {err}"
        );
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            original,
            "file must be byte-identical after a refused edit"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(feature = "ast")]
    #[test]
    fn edit_file_allows_valid_rust_edit() {
        // A syntactically valid rewrite passes the gate and is committed.
        let root = test_root("edit_syntax_ok");
        let file = root.join("lib.rs");
        let original = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        fs::write(&file, original).unwrap();
        let ex = executor(&root);

        let out = ex
            .edit_file(
                "lib.rs",
                "fn add(a: i32, b: i32) -> i32 {",
                "fn add_two(a: i32, b: i32) -> i32 {",
                false,
            )
            .unwrap();
        assert!(out.contains("Replaced 1 occurrence(s)"));
        let after = fs::read_to_string(&file).unwrap();
        assert!(after.contains("fn add_two("));
        assert!(after.contains("a + b"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_dir_caps_at_100_items() {
        let root = test_root("list_cap");
        let sub = root.join("files");
        fs::create_dir_all(&sub).unwrap();
        for i in 0..105 {
            fs::write(sub.join(format!("file_{i:03}.txt")), "data").unwrap();
        }
        let ex = executor(&root);
        let out = ex.list_dir("files", 1).unwrap();
        assert!(out.contains("100 items"));
        assert!(out.contains("list truncated: maximum 100 items reached"));
    }

    #[test]
    fn search_code_truncates_long_matched_lines() {
        let root = test_root("search_long_line");
        let file = root.join("bundle.js");
        let long_line = format!("const x = 'hello'; {}", "a".repeat(1000));
        fs::write(&file, &long_line).unwrap();
        let ex = executor(&root);
        let out = ex.search_code("hello", "bundle.js", 10).unwrap();
        assert!(out.contains("match(es) for 'hello'"));
        assert!(out.contains("... [line truncated]"));
        assert!(out.len() < 500);
    }

    /// The fallback `search_code` path (filesystem walk) must return
    /// matches in a deterministic order that is independent of the platform's
    /// raw directory-enumeration order. `walk_search` collects each directory's
    /// entries into a `Vec` and sorts by `file_name()` before recursing, so the
    /// emitted order is stable (depth-first, siblings sorted).
    #[test]
    fn fallback_search_order_is_deterministic_sorted_by_filename() {
        let root = test_root("search_order");
        // Create in a non-alphabetical order so a naive enumeration-order
        // assertion would fail; the sorted walk must still come out ordered.
        for n in ["zeta.txt", "alpha.txt", "mid.txt"] {
            fs::write(root.join(n), format!("line with needle-{}\n", n)).unwrap();
        }
        let ex = executor(&root);
        let out = ex.fallback_search("needle", &root, 100).unwrap();
        assert_eq!(out.lines().next().unwrap(), "3 match(es) for 'needle':");
        let a = out.find("alpha.txt").unwrap();
        let m = out.find("mid.txt").unwrap();
        let z = out.find("zeta.txt").unwrap();
        assert!(
            a < m && m < z,
            "expected alpha < mid < zeta (sorted by file_name), got:\n{out}"
        );
        // Run a second time to confirm determinism across repeated invocations.
        let out2 = ex.fallback_search("needle", &root, 100).unwrap();
        assert_eq!(out, out2, "fallback search order must be deterministic");
    }

    #[test]
    fn read_file_truncates_oversized_single_line() {
        let root = test_root("read_long_line");
        let file = root.join("min.js");
        let long_line = "b".repeat(5000);
        fs::write(&file, &long_line).unwrap();
        let ex = executor(&root);
        let out = ex.read_file("min.js", 1, None).unwrap();
        assert!(out.contains("... [line truncated]"));
        assert!(out.len() < 3000);
    }

    #[test]
    fn read_file_inverted_bounds_auto_swaps_and_does_not_panic() {
        let root = test_root("read_inverted_bounds");
        let file = root.join("test.txt");
        let content = (1..=50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&file, &content).unwrap();
        let ex = executor(&root);

        // Inverted bounds: start 30, end 10
        let out = ex.read_file("test.txt", 30, Some(10)).unwrap();
        assert!(out.contains("showing: 10-30"));
        assert!(out.contains("10: line 10"));
        assert!(out.contains("30: line 30"));
        assert!(!out.contains("9: line 9"));
        assert!(!out.contains("31: line 31"));

        // Single line when start == end
        let out_single = ex.read_file("test.txt", 15, Some(15)).unwrap();
        assert!(out_single.contains("showing: 15-15"));
        assert!(out_single.contains("15: line 15"));

        // Out of bounds start
        let out_oob = ex.read_file("test.txt", 100, Some(200)).unwrap();
        assert!(out_oob.contains("showing: 50-50"));
    }

    /// Rewriting an existing file must preserve its mode (e.g. 0644), not
    /// downgrade it to the `tempfile` 0600 default. A brand-new file keeps the
    /// 0600 tempfile default (nothing to preserve).
    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = unix_test_root("atomic_perms");
        let file = root.join("perm.txt");
        fs::write(&file, "original\n").unwrap();
        let mut p = fs::metadata(&file).unwrap().permissions();
        p.set_mode(0o644);
        fs::set_permissions(&file, p).unwrap();
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644);

        let ex = executor(&root);
        // Rewrite the existing file: its 0644 mode must survive.
        ex.write_file("perm.txt", "rewritten\n", true).unwrap();
        let after = fs::metadata(&file).unwrap().permissions();
        assert_eq!(
            after.mode() & 0o777,
            0o644,
            "rewrite must preserve the original 0644 mode"
        );

        // A brand-new file has no prior mode to preserve, so it stays at the
        // tempfile default (0600) rather than being promoted to 0644.
        let fresh = root.join("fresh.txt");
        ex.write_file("fresh.txt", "new\n", true).unwrap();
        assert_eq!(
            fs::metadata(&fresh).unwrap().permissions().mode() & 0o777,
            0o600,
            "a new file must keep the 0600 tempfile default"
        );
    }

    /// `read_file` must refuse non-regular files (FIFOs, sockets, devices, and
    /// directories) with `InvalidArgs` instead of blocking on an unbounded read.
    #[cfg(unix)]
    #[test]
    fn read_file_rejects_non_regular_files() {
        use std::os::unix::fs::PermissionsExt;
        let root = unix_test_root("nonregular_read");
        let ex = executor(&root);

        // FIFO: opening it for read would block the worker thread.
        let fifo = root.join("pipe.fifo");
        let cstr = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        let perms = std::fs::Permissions::from_mode(0o644);
        let rc = unsafe { libc::mkfifo(cstr.as_ptr(), perms.mode()) };
        assert_eq!(rc, 0, "libc::mkfifo should succeed");
        let err = ex.read_file("pipe.fifo", 1, None).unwrap_err();
        assert!(
            matches!(err, FsError::InvalidArgs(ref msg) if msg.contains("non-regular file")),
            "FIFO must be refused with InvalidArgs, got {err:?}"
        );

        // Directory: non-regular file, refused with the same guard.
        let d = root.join("adirectory");
        fs::create_dir_all(&d).unwrap();
        let err_d = ex.read_file("adirectory", 1, None).unwrap_err();
        assert!(
            matches!(err_d, FsError::InvalidArgs(ref msg) if msg.contains("non-regular file")),
            "directory must be refused with InvalidArgs, got {err_d:?}"
        );
    }

    /// Searching with a *file* path (non-git workspace) must return matches from
    /// that file only, never from sibling files, even though a directory search
    /// would return both.
    #[test]
    fn search_code_constrained_to_single_file() {
        let root = test_root("search_single_file");
        fs::write(root.join("alpha.txt"), "alpha NEEDLE line\n").unwrap();
        fs::write(root.join("beta.txt"), "beta NEEDLE line\n").unwrap();
        let ex = executor(&root);

        // Directory search finds both siblings.
        let dir_out = ex.search_code("NEEDLE", ".", 100).unwrap();
        assert!(dir_out.contains("alpha.txt"), "{dir_out}");
        assert!(dir_out.contains("beta.txt"), "{dir_out}");

        // File search is constrained to that one file.
        let file_out = ex.search_code("NEEDLE", "alpha.txt", 100).unwrap();
        assert!(file_out.contains("alpha.txt"), "{file_out}");
        assert!(
            !file_out.contains("beta.txt"),
            "sibling file must not appear in a single-file search: {file_out}"
        );
        assert!(file_out.contains("1 match(es)"), "{file_out}");
    }

    /// Same as above but inside a git repo, exercising the `git grep` pathspec
    /// branch (`git_grep_file`) rather than the filesystem-walk fallback.
    #[test]
    fn search_code_constrained_to_single_file_in_git_repo() {
        let root = test_root("search_single_file_git");
        fs::write(root.join("alpha.txt"), "alpha NEEDLE line\n").unwrap();
        fs::write(root.join("beta.txt"), "beta NEEDLE line\n").unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(init.status.success(), "git init failed: {init:?}");

        let ex = executor(&root);

        // Directory search (git grep --untracked) finds both siblings.
        let dir_out = ex.search_code("NEEDLE", ".", 100).unwrap();
        assert!(dir_out.contains("alpha.txt"), "{dir_out}");
        assert!(dir_out.contains("beta.txt"), "{dir_out}");

        // File search uses the file as the git grep pathspec → only that file.
        let file_out = ex.search_code("NEEDLE", "alpha.txt", 100).unwrap();
        assert!(file_out.contains("alpha.txt"), "{file_out}");
        assert!(
            !file_out.contains("beta.txt"),
            "sibling file must not appear in a single-file git search: {file_out}"
        );
        assert!(file_out.contains("1 match(es)"), "{file_out}");
    }
}
