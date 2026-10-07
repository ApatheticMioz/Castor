//! Structural AST search/replace powered by `ast-grep` (tree-sitter).
//!
//! Port of `mcp-castor/src/harness/services/ast_service.js` semantics:
//! - `ast_search` — code-like pattern matching with metavariables (`$VAR`, `$$$BODY`).
//! - `ast_replace` — structural rewrite that preserves formatting, with a
//!   mandatory syntax gate (reparse + ERROR-node check) and per-file rollback.
//!
//! All paths flow through the sandbox layers in [`super::sandbox`].
//!
//! Note on the "parse error" gate: tree-sitter always produces a tree (with
//! error recovery), so a malformed rewrite does not fail to parse — it
//! produces a tree containing `ERROR`/`UNDEFINED` nodes. The syntax gate
//! therefore reparses the rewritten content and treats the presence of any
//! ERROR-kind node as "verified invalid" → rollback.

use std::fs;
use std::path::{Path, PathBuf};

use ast_grep_core::matcher::Pattern;
use ast_grep_core::tree_sitter::LanguageExt;
use ast_grep_core::{Language, Node};
use ast_grep_language::SupportLang;
use thiserror::Error;

use super::sandbox::{self, SandboxError};

/// Directories skipped during recursive walks (mirrors `sandbox_fs.js`).
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

/// Files larger than this are skipped (implausible for AST).
const MAX_FILE_BYTES: u64 = 1_000_000;

/// Global cap on matches returned by a search (single file or directory scan).
const MAX_MATCHES: usize = 50;

#[derive(Debug, Error)]
pub enum AstError {
    #[error("{0}")]
    Sandbox(#[from] SandboxError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    InvalidArgs(String),
    #[error("{0}")]
    Pattern(String),
    #[error("{0}")]
    Language(String),
}

/// A single AST match. `line`/`col` are 0-indexed (matching the JS contract).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub text: String,
}

/// Summary of a batch replace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplaceSummary {
    pub files_scanned: usize,
    pub files_applied: usize,
    pub files_rolled_back: usize,
    pub replacements: usize,
    pub applied: Vec<String>,
    pub rolled_back: Vec<String>,
}

/// Resolve a file path to its canonical `SupportLang` via
/// `SupportLang::from_path`, which knows the full 28-language extension map
/// (e.g. `.go` → Go, `.sh` → Bash, `.json` → Json, `.rs` → Rust). Returns
/// `None` for unrecognized extensions (the file is skipped on directory scans).
fn infer_lang_by_extension(path: &Path) -> Option<SupportLang> {
    SupportLang::from_path(path)
}

/// Parse a language alias (e.g. `"js"`, `"ts"`, `"py"`, `"rs"`) into a
/// `SupportLang`. All 28 canonical aliases from `SupportLang::all_langs` are
/// accepted (case-insensitively).
fn parse_lang(alias: &str) -> Result<SupportLang, AstError> {
    alias
        .parse::<SupportLang>()
        .map_err(|e| AstError::Language(format!("unsupported language '{alias}': {e}")))
}

/// Recursively collect candidate files under `dir`, skipping ignored
/// directories, over-large files, and files whose language cannot be inferred.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if IGNORED_DIRS.contains(&name.as_str()) {
            continue;
        }
        let full = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            collect_files(&full, out);
        } else if ft.is_file() {
            let size = match entry.metadata() {
                Ok(m) => m.len(),
                Err(_) => continue,
            };
            if size > MAX_FILE_BYTES {
                continue;
            }
            if infer_lang_by_extension(&full).is_none() {
                continue;
            }
            out.push(full);
        }
    }
}

/// Resolve a search/replace target to a list of files to process.
/// A file target yields a single-element list; a directory target is walked.
fn resolve_files(target: &Path) -> Result<Vec<PathBuf>, AstError> {
    if !target.exists() {
        return Err(AstError::InvalidArgs(format!(
            "Path does not exist: {}",
            target.display()
        )));
    }
    if target.is_file() {
        return Ok(vec![target.to_path_buf()]);
    }
    let mut files = Vec::new();
    collect_files(target, &mut files);
    files.sort();
    Ok(files)
}

/// Run the sandbox containment layers for a raw target path string, returning
/// the resolved in-tree path.
fn resolve_target(root: &Path, raw: &str) -> Result<PathBuf, AstError> {
    let resolved_root = sandbox::resolve_workspace_root(root)?;
    let normalized = sandbox::normalize_traversal(&resolved_root, raw)?;
    sandbox::refuse_out_of_tree(&resolved_root, &normalized)?;
    sandbox::verify_symlink_containment(&normalized, &resolved_root)?;
    Ok(normalized)
}

/// Search for a syntactic pattern under `root` (or within `path` if specified).
///
/// `lang` is a target language alias from the 28-language `SupportLang` set
/// (e.g. `"ts"`, `"js"`, `"py"`, `"go"`, `"rs"`). Files whose extension maps to
/// a different language are skipped.
pub fn ast_search(
    root: &Path,
    pattern: &str,
    lang: &str,
    path: Option<&str>,
) -> Result<Vec<Match>, AstError> {
    if pattern.trim().is_empty() {
        return Err(AstError::InvalidArgs(
            "AST search requires a non-empty pattern".into(),
        ));
    }
    let raw = match path {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => &root.to_string_lossy(),
    };
    let target = resolve_target(root, raw)?;
    let files = resolve_files(&target)?;

    let target_lang = parse_lang(lang)?;
    let pat =
        Pattern::try_new(pattern, target_lang).map_err(|e| AstError::Pattern(e.to_string()))?;

    let mut matches = Vec::new();
    for file in &files {
        if matches.len() >= MAX_MATCHES {
            break;
        }
        let file_lang = infer_lang_by_extension(file).unwrap_or(target_lang);
        if file_lang != target_lang {
            continue;
        }
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let doc = target_lang.ast_grep(&content);
        for m in doc.root().find_all(&pat) {
            if matches.len() >= MAX_MATCHES {
                break;
            }
            let pos = m.start_pos();
            matches.push(Match {
                file: file.to_string_lossy().to_string(),
                line: pos.line(),
                col: pos.column(m.get_node()),
                text: m.text().to_string(),
            });
        }
    }
    Ok(matches)
}

/// Replace a syntactic pattern under `root` (or within `path` if specified), applying
/// the rewrite per file with a syntax gate (reparse + ERROR-node check) and per-file rollback.
///
/// `lang` is the target language alias. Files whose language does not match
/// `lang` are skipped.
pub fn ast_replace(
    root: &Path,
    pattern: &str,
    replacement: &str,
    lang: &str,
    path: Option<&str>,
) -> Result<ReplaceSummary, AstError> {
    if pattern.trim().is_empty() {
        return Err(AstError::InvalidArgs(
            "AST replace requires a non-empty pattern".into(),
        ));
    }
    let raw = match path {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => &root.to_string_lossy(),
    };
    let target = resolve_target(root, raw)?;
    let files = resolve_files(&target)?;
    let target_lang = parse_lang(lang)?;

    // Validate pattern syntax upfront.
    Pattern::try_new(pattern, target_lang).map_err(|e| AstError::Pattern(e.to_string()))?;

    let mut summary = ReplaceSummary {
        files_scanned: files.len(),
        ..Default::default()
    };

    for file in &files {
        let file_lang = infer_lang_by_extension(file).unwrap_or(target_lang);
        if file_lang != target_lang {
            continue;
        }

        let original = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let result = replace_in_memory(&original, pattern, replacement, target_lang);
        match result {
            Ok((new_content, replacements)) => {
                if new_content == original {
                    continue; // no match -> not counted
                }
                // Syntax gate: reparse the new content; any ERROR node -> rollback.
                if syntax_gate(&new_content, target_lang) {
                    // Verified invalid: leave the file unchanged (never written).
                    summary.files_rolled_back += 1;
                    summary.rolled_back.push(file.to_string_lossy().to_string());
                    continue;
                }
                if let Err(e) = fs::write(file, &new_content) {
                    summary.files_rolled_back += 1;
                    summary.rolled_back.push(file.to_string_lossy().to_string());
                    let _ = e;
                    continue;
                }
                summary.files_applied += 1;
                summary.replacements += replacements;
                summary.applied.push(file.to_string_lossy().to_string());
            }
            Err(e) => {
                // Parse/pattern failure on this file -> skip, continue the batch.
                let _ = e;
                continue;
            }
        }
    }

    Ok(summary)
}

/// Compute the rewrite fully in memory. Returns `(new_content, replacements)`.
fn replace_in_memory(
    original: &str,
    pattern: &str,
    replacement: &str,
    lang: SupportLang,
) -> Result<(String, usize), AstError> {
    let doc = lang.ast_grep(original);
    let pat = Pattern::try_new(pattern, lang).map_err(|e| AstError::Pattern(e.to_string()))?;

    // Collect the edits as owned values (they do not borrow the doc), so the
    // matches can be dropped before we mutate the source.
    let edits: Vec<ast_grep_core::source::Edit<String>> = doc
        .root()
        .find_all(pat)
        .map(|m| m.replace_by(replacement))
        .collect();
    if edits.is_empty() {
        return Ok((original.to_string(), 0));
    }

    // Splice the edits in reverse source order so earlier offsets stay valid.
    let mut src = original.to_string();
    for edit in edits.iter().rev() {
        let end = edit.position + edit.deleted_length;
        src.replace_range(
            edit.position..end,
            &String::from_utf8_lossy(&edit.inserted_text),
        );
    }

    Ok((src, edits.len()))
}

/// Syntax gate: reparse `content` and report whether any ERROR-kind node is
/// present (tree-sitter error recovery). Presence means the rewrite is
/// malformed and the file must be rolled back.
fn syntax_gate(content: &str, lang: SupportLang) -> bool {
    let doc = lang.ast_grep(content);
    let mut has_error = false;
    walk_for_errors(&doc.root(), &mut has_error);
    has_error
}

fn walk_for_errors(node: &Node<ast_grep_core::tree_sitter::StrDoc<SupportLang>>, out: &mut bool) {
    if *out {
        return;
    }
    let kind = node.kind();
    if kind == "ERROR" || kind == "UNDEFINED" {
        *out = true;
        return;
    }
    for child in node.children() {
        walk_for_errors(&child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("castor_ast_{}_{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        dunce::canonicalize(&root).unwrap_or(root)
    }

    #[test]
    fn search_finds_pattern_in_fixture() {
        let root = test_root("search");
        let file = root.join("src").join("app.ts");
        fs::write(&file, "function greet(name) {\n  return `hi ${name}`;\n}\n").unwrap();

        let matches = ast_search(&root, "function $NAME($$$ARGS) { $$$BODY }", "ts", None).unwrap();
        assert!(!matches.is_empty(), "expected at least one match");
        let m = &matches[0];
        assert_eq!(m.file, file.to_string_lossy());
        assert_eq!(m.line, 0);
        assert_eq!(m.col, 0);
        assert!(m.text.contains("greet"), "match text: {}", m.text);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_finds_pattern_in_rust_fixture() {
        let root = test_root("search_rust");
        let file = root.join("src").join("lib.rs");
        fs::write(
            &file,
            "fn compute_sum(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();

        let matches =
            ast_search(&root, "fn $NAME($$$ARGS) -> $RET { $$$BODY }", "rs", None).unwrap();
        assert!(!matches.is_empty(), "expected at least one match");
        let m = &matches[0];
        assert_eq!(m.file, file.to_string_lossy());
        assert!(m.text.contains("compute_sum"), "match text: {}", m.text);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_skips_unrelated_language_files() {
        let root = test_root("mixed_langs");
        let ts_file = root.join("src").join("app.ts");
        let rs_file = root.join("src").join("lib.rs");
        fs::write(
            &ts_file,
            "function greet(name: string): string {\n  return `hi ${name}`;\n}\n",
        )
        .unwrap();
        fs::write(
            &rs_file,
            "fn compute_sum(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
        )
        .unwrap();

        // Searching Rust pattern must succeed across mixed dir, ignoring the .ts file without error.
        let rs_matches =
            ast_search(&root, "fn $NAME($$$ARGS) -> $RET { $$$BODY }", "rs", None).unwrap();
        assert_eq!(rs_matches.len(), 1);
        assert_eq!(rs_matches[0].file, rs_file.to_string_lossy());

        // Searching TS pattern must succeed across mixed dir, ignoring the .rs file without error.
        let ts_matches = ast_search(
            &root,
            "function $NAME($$$ARGS): $RET { $$$BODY }",
            "ts",
            None,
        )
        .unwrap();
        assert_eq!(ts_matches.len(), 1);
        assert_eq!(ts_matches[0].file, ts_file.to_string_lossy());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn search_resolves_representative_languages() {
        // Prove extension→SupportLang resolution + parsing across a
        // representative spread of the bundled grammars.
        let root = test_root("representative");
        // (relative fixture path, language alias, ast-grep pattern, expected match text, source)
        let cases: &[(&str, &str, &str, &str, &str)] = &[
            (
                "src/greet.go",
                "go",
                "return a + b",
                "a + b",
                "package main\n\nfunc add(a int, b int) int {\n\treturn a + b\n}\n",
            ),
            (
                "src/add.c",
                "c",
                "return a + b;",
                "a + b",
                "int add(int a, int b) {\n    return a + b;\n}\n",
            ),
            (
                "src/add.cpp",
                "cpp",
                "return a + b;",
                "a + b",
                "int add(int a, int b) {\n    return a + b;\n}\n",
            ),
            (
                "src/Program.cs",
                "cs",
                "System.Console.WriteLine($MSG);",
                "hi",
                "class Program {\n    void Main() {\n        System.Console.WriteLine(\"hi\");\n    }\n}\n",
            ),
            (
                "src/App.java",
                "java",
                "System.out.println($MSG);",
                "hi",
                "class App {\n    public void run() {\n        System.out.println(\"hi\");\n    }\n}\n",
            ),
            (
                "src/greet.sh",
                "bash",
                "echo $MSG",
                "hi",
                "#!/bin/sh\ngreet() {\n    echo \"hi\"\n}\n",
            ),
            (
                "src/meta.json",
                "json",
                "{ $$$ }",
                "\"name\"",
                "{\n    \"name\": \"castor\",\n    \"version\": \"1.0.2\"\n}\n",
            ),
            (
                "src/add.py",
                "py",
                "return a + b",
                "a + b",
                "def add(a, b):\n    return a + b\n",
            ),
            (
                "src/add.rs",
                "rs",
                "a + b",
                "a + b",
                "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
            ),
        ];

        for (rel, _, _, _, src) in cases {
            let file = root.join(rel);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, src).unwrap();
        }

        for (rel, lang, pattern, expected_text, _) in cases {
            let expected = root.join(rel);
            let matches = ast_search(&root, pattern, lang, None).unwrap();
            assert!(
                !matches.is_empty(),
                "language '{lang}' ({rel}) should match pattern: {pattern}"
            );
            assert_eq!(
                std::path::Path::new(&matches[0].file),
                expected.as_path(),
                "language '{lang}' resolved to wrong file: {}",
                matches[0].file
            );
            assert!(
                matches[0].text.contains(expected_text),
                "language '{lang}' match text should contain '{expected_text}', got: {}",
                matches[0].text
            );
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn infer_lang_by_extension_maps_representative_extensions() {
        // Direct check of the canonical extension→SupportLang mapping.
        let table: &[(&str, SupportLang)] = &[
            ("a.go", SupportLang::Go),
            ("a.c", SupportLang::C),
            ("a.cpp", SupportLang::Cpp),
            ("a.cs", SupportLang::CSharp),
            ("a.java", SupportLang::Java),
            ("a.sh", SupportLang::Bash),
            ("a.json", SupportLang::Json),
            ("a.py", SupportLang::Python),
            ("a.rs", SupportLang::Rust),
            ("a.ts", SupportLang::TypeScript),
            ("a.tsx", SupportLang::Tsx),
            ("a.md", SupportLang::Markdown),
            ("a.yaml", SupportLang::Yaml),
        ];
        for (name, expected) in table {
            let path = std::path::Path::new(name);
            let lang = infer_lang_by_extension(path)
                .unwrap_or_else(|| panic!("extension '{name}' should resolve"));
            assert_eq!(lang, *expected, "extension '{name}' mis-mapped");
        }

        // Unknown extension stays out of the supported set.
        assert_eq!(
            infer_lang_by_extension(std::path::Path::new("a.xyz_unknown")),
            None
        );
    }

    #[test]
    fn search_targets_specific_file_path() {
        let root = test_root("search_path");
        let file_a = root.join("src").join("a.ts");
        let file_b = root.join("src").join("b.ts");
        fs::write(&file_a, "function foo() { return 1; }\n").unwrap();
        fs::write(&file_b, "function bar() { return 2; }\n").unwrap();

        // Target ONLY a.ts
        let matches = ast_search(
            &root,
            "function $NAME() { $$$BODY }",
            "ts",
            Some("src/a.ts"),
        )
        .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, file_a.to_string_lossy());
        assert!(matches[0].text.contains("foo"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn replace_applies_and_reparses_clean() {
        let root = test_root("replace_ok");
        let file = root.join("src").join("app.ts");
        let original = "function greet(name) {\n  return `hi ${name}`;\n}\n";
        fs::write(&file, original).unwrap();

        let summary = ast_replace(
            &root,
            "function $NAME($$$ARGS) { $$$BODY }",
            "function renamed($$$ARGS) { $$$BODY }",
            "ts",
            None,
        )
        .unwrap();

        assert_eq!(summary.files_applied, 1, "expected one applied file");
        assert_eq!(summary.files_rolled_back, 0, "expected no rollbacks");
        assert_eq!(summary.replacements, 1);
        assert_eq!(summary.applied, vec![file.to_string_lossy().to_string()]);

        let new_content = fs::read_to_string(&file).unwrap();
        assert!(new_content.contains("renamed"), "content: {new_content}");
        assert!(!new_content.contains("greet"), "content: {new_content}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn replace_targets_specific_file_path() {
        let root = test_root("replace_path");
        let file_a = root.join("src").join("a.ts");
        let file_b = root.join("src").join("b.ts");
        fs::write(&file_a, "function target() {}\n").unwrap();
        fs::write(&file_b, "function target() {}\n").unwrap();

        // Replace ONLY in a.ts
        let summary = ast_replace(
            &root,
            "function target() {}",
            "function updated() {}",
            "ts",
            Some("src/a.ts"),
        )
        .unwrap();

        assert_eq!(summary.files_applied, 1);
        assert_eq!(summary.applied, vec![file_a.to_string_lossy().to_string()]);
        assert_eq!(
            fs::read_to_string(&file_a).unwrap(),
            "function updated() {}\n"
        );
        assert_eq!(
            fs::read_to_string(&file_b).unwrap(),
            "function target() {}\n"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_replacement_rolls_back() {
        let root = test_root("replace_bad");
        let file = root.join("src").join("app.ts");
        let original = "function greet(name) {\n  return `hi ${name}`;\n}\n";
        fs::write(&file, original).unwrap();

        // A replacement that produces malformed syntax (a dangling `function`
        // with no body) must be rolled back: the file stays unchanged and the
        // file is named in the rolled-back list.
        let summary = ast_replace(
            &root,
            "function $NAME($$$ARGS) { $$$BODY }",
            "function broken(",
            "ts",
            None,
        )
        .unwrap();

        assert_eq!(
            summary.files_rolled_back, 1,
            "expected one rolled-back file"
        );
        assert_eq!(summary.files_applied, 0);
        assert_eq!(
            summary.rolled_back,
            vec![file.to_string_lossy().to_string()],
            "rolled-back list must name the file"
        );

        let after = fs::read_to_string(&file).unwrap();
        assert_eq!(after, original, "file must be unchanged after rollback");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn out_of_tree_root_refused() {
        // A non-existent absolute root is refused by the sandbox before any
        // AST work happens.
        let missing = std::env::temp_dir().join("castor_ast_missing_never");
        let err = ast_search(&missing, "function $N() {}", "ts", None).unwrap_err();
        assert!(
            matches!(err, AstError::Sandbox(SandboxError::WorkspaceRoot(_))),
            "expected a sandbox refusal, got {err:?}"
        );
    }
}
