//! Tool output spilling and deterministic workspace skeleton generation.

use std::fs;
use std::path::{Path, PathBuf};

pub const SPILL_THRESHOLD_BYTES: usize = 16384;

pub fn spill_tool_output(
    output: &str,
    tool_call_id: &str,
    scratch_root: Option<&Path>,
    threshold: Option<usize>,
) -> (String, Option<(usize, PathBuf)>) {
    let threshold = threshold.unwrap_or(SPILL_THRESHOLD_BYTES);
    let bytes = output.len();
    if bytes <= threshold {
        return (output.to_string(), None);
    }

    let safe_id: String = tool_call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let safe_id = if safe_id.is_empty() {
        "out".to_string()
    } else {
        safe_id
    };

    let fallback_scratch = std::env::temp_dir().join("castor_scratch");
    let base = scratch_root.unwrap_or(&fallback_scratch);
    let scratch_dir = base.join(".scratch");
    let _ = fs::create_dir_all(&scratch_dir);
    let _ = crate::pruner::prune_scratch_dir(
        &scratch_dir,
        crate::pruner::DEFAULT_SCRATCH_TTL_MS,
        None,
    );
    let file_path = scratch_dir.join(format!("tool_out_{safe_id}.txt"));

    match fs::write(&file_path, output) {
        Ok(()) => {
            let reported = dunce::canonicalize(&file_path).unwrap_or(file_path);
            let head: String = output.chars().take(1024).collect();
            let tail: String = output
                .chars()
                .rev()
                .take(1024)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            let elided = bytes.saturating_sub(head.len() + tail.len());
            let pointer = format!(
                "[Tool output spilled to disk: {bytes} bytes > {threshold}-byte threshold. \
Full payload saved to: {}]\n\n\
--- Head preview (first 1024 bytes) ---\n{head}\n\
... [{elided} bytes elided] ...\n\
--- Tail preview (last 1024 bytes) ---\n{tail}\n\n\
Hint: use read_file with start_line/end_line or search_code to inspect specific regions of {}.",
                reported.display(),
                reported.display()
            );
            (pointer, Some((bytes, reported)))
        }
        Err(e) => {
            let head: String = output.chars().take(2048).collect();
            let elided = bytes.saturating_sub(head.len());
            let truncated = format!(
                "[ToolOutputSpillError: could not write to {}: {e}. Payload truncated from {bytes} bytes]\n\n{head}\n\n... [remaining {elided} bytes truncated] ...",
                file_path.display()
            );
            (truncated, None)
        }
    }
}

const WORKSPACE_SKELETON_MAX_ENTRIES: usize = 40;

const WORKSPACE_SKELETON_IGNORED: &[&str] = &[".git", "target", "node_modules", ".scratch"];

const WORKSPACE_SKELETON_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "AGENTS.md",
    "CLAUDE.md",
    "GEMINI.md",
    "README.md",
];

pub fn build_workspace_skeleton(workspace: &Path) -> String {
    let mut out = String::from("[Workspace Skeleton]\n");
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    if let Ok(iter) = fs::read_dir(workspace) {
        for entry in iter.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_hidden = name.starts_with('.');
            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir {
                if !is_hidden && !WORKSPACE_SKELETON_IGNORED.contains(&name.as_str()) {
                    dirs.push(name);
                }
            } else if WORKSPACE_SKELETON_MANIFESTS.contains(&name.as_str()) {
                files.push(name);
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    files.sort();
    files.dedup();
    for d in dirs.iter().take(WORKSPACE_SKELETON_MAX_ENTRIES) {
        out.push_str(&format!("- {d}/\n"));
    }
    let more_dirs = dirs.len().saturating_sub(WORKSPACE_SKELETON_MAX_ENTRIES);
    for f in &files {
        out.push_str(&format!("- {f}\n"));
    }
    if more_dirs > 0 {
        out.push_str(&format!("({more_dirs} more…)\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_spill_below_threshold_returns_unchanged() {
        let (out, spilled) = spill_tool_output("short", "id1", None, Some(100));
        assert_eq!(out, "short");
        assert!(spilled.is_none());
    }

    #[test]
    fn tool_output_spill_above_threshold_spills_to_scratch() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = "x".repeat(200);
        let (out, spilled) = spill_tool_output(&payload, "call_123", Some(tmp.path()), Some(100));
        assert!(out.contains("[Tool output spilled to disk: 200 bytes > 100-byte threshold."));
        assert!(spilled.is_some());
        let (bytes, path) = spilled.unwrap();
        assert_eq!(bytes, 200);
        assert!(path.exists());
        assert_eq!(fs::read_to_string(path).unwrap(), payload);
    }

    #[test]
    fn build_workspace_skeleton_is_deterministic_sorted_and_excludes_build_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]").unwrap();
        fs::write(root.join("random_output.mp4"), "binary").unwrap();

        let skeleton = build_workspace_skeleton(root);
        assert!(skeleton.contains("- src/\n"));
        assert!(!skeleton.contains("- target/"));
        assert!(!skeleton.contains("- .git/"));
        assert!(skeleton.contains("- Cargo.toml\n"));
        assert!(!skeleton.contains("random_output.mp4"));
    }
}

