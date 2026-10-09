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

struct WalkContext<'a> {
    base: &'a Path,
    max_depth: usize,
    items: Vec<(String, String)>,
    partial: bool,
    truncated: bool,
    max_items: usize,
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
        let meta = fs::metadata(&resolved)?;
        if meta.is_dir() {
            return Err(FsError::InvalidArgs(format!(
                "Path is a directory, not a file: {path}"
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
        let start = start_line.saturating_sub(1);
        let end = end_line
            .map(|e| e.min(total))
            .unwrap_or_else(|| (start + 800).min(total));
        let slice = &lines[start.min(total)..end];
        let numbered: Vec<String> = slice
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let content = truncate_line(line, MAX_READ_LINE_CHARS);
                format!("{}: {}", start + i + 1, content)
            })
            .collect();
        Ok(format!(
            "{} (total: {} lines, showing: {}-{})\n{}",
            resolved.display(),
            total,
            start + 1,
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
        use std::fmt::Write;
        let mut diff = String::from("\n```diff\n");
        let target_lines: Vec<&str> = target.lines().collect();
        for line in target_lines.iter().take(6) {
            let _ = writeln!(diff, "- {line}");
        }
        if target_lines.len() > 6 {
            diff.push_str("  ...\n");
        }
        let repl_lines: Vec<&str> = replacement.lines().collect();
        for line in repl_lines.iter().take(6) {
            let _ = writeln!(diff, "+ {line}");
        }
        if repl_lines.len() > 6 {
            diff.push_str("  ...\n");
        }
        diff.push_str("```");
        diff
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

        let (effective_target, local_style) = if original.contains(target) {
            (target.to_string(), line_ending_style(target))
        } else if crlf_target != target && original.contains(&crlf_target) {
            (crlf_target, Some("crlf"))
        } else if lf_target != target && original.contains(&lf_target) {
            (lf_target, Some("lf"))
        } else {
            return Err(FsError::EditError(format!(
                "Target content not found in file: {path}. No write performed."
            )));
        };

        let count = original.matches(&effective_target).count();
        if count == 0 {
            return Err(FsError::EditError(format!(
                "Target content not found in file: {path}. No write performed."
            )));
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

        let mut ctx = WalkContext {
            base: &resolved,
            max_depth,
            items: Vec::new(),
            partial: false,
            truncated: false,
            max_items: 100,
        };
        self.walk(&resolved, 1, &mut ctx)?;

        use std::fmt::Write;
        let mut out = format!("{} ({} items):\n", resolved.display(), ctx.items.len());
        for (kind, rel) in &ctx.items {
            let _ = writeln!(out, "  [{kind}] {rel}");
        }
        if ctx.truncated {
            out.push_str("  ... [list truncated: maximum 100 items reached; specify deeper path or lower max_depth]\n");
        }
        if ctx.partial {
            out.push_str("  [partial: some directories were unreadable]\n");
        }
        Ok(out)
    }

    fn walk(&self, current: &Path, depth: usize, ctx: &mut WalkContext<'_>) -> Result<(), FsError> {
        if depth > ctx.max_depth || ctx.truncated {
            return Ok(());
        }
        let entries = match fs::read_dir(current) {
            Ok(e) => e,
            Err(_) => {
                ctx.partial = true;
                return Ok(());
            }
        };
        for entry in entries {
            if ctx.items.len() >= ctx.max_items {
                ctx.truncated = true;
                break;
            }
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }
            let full = entry.path();
            let rel = full.strip_prefix(ctx.base).unwrap_or(&full);
            let rel_str = rel.to_string_lossy().to_string();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                ctx.items.push(("dir".into(), rel_str));
                self.walk(&full, depth + 1, ctx)?;
            } else {
                ctx.items.push(("file".into(), rel_str));
            }
        }
        Ok(())
    }

    fn search_code(&self, query: &str, path: &str, max_results: usize) -> Result<String, FsError> {
        let resolved = self.resolve_read(path)?;
        let search_dir = if resolved.is_file() {
            resolved
                .parent()
                .unwrap_or_else(|| self.root())
                .to_path_buf()
        } else {
            resolved
        };

        if self.is_inside_git_repo(&search_dir) {
            return self.git_grep(query, &search_dir, max_results);
        }
        self.fallback_search(query, &search_dir, max_results)
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
        let rel = search_dir.strip_prefix(self.root()).unwrap_or(search_dir);
        let rel_str = rel.to_string_lossy().to_string();

        let mut args = vec!["grep", "-n", "-I", "-F", "--untracked", "-e", query];
        if !rel_str.is_empty() && rel_str != "." {
            args.push("--");
            args.push(&rel_str);
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
        let lines: Vec<&str> = stdout.lines().collect();
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
        let entries = match fs::read_dir(current) {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        for entry in entries {
            if matches.len() >= max_results {
                break;
            }
            let entry = entry?;
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
        let parent = target.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(
            ".castor_tmp_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&tmp, content)?;
        if let Err(e) = fs::rename(&tmp, target) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
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

        // Directory with image extension is rejected
        let img_dir = root.join("folder.webp");
        fs::create_dir_all(&img_dir).unwrap();
        let err_dir = ex.read_file("folder.webp", 1, None).unwrap_err();
        assert!(matches!(err_dir, FsError::InvalidArgs(msg) if msg.contains("is a directory")));
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
}
