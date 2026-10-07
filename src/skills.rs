//! Skill definitions and loading (agentskills.io SKILL.md layer).
//!
//! A "skill" is a reusable workflow recipe stored at:
//!
//! ```text
//! <dir>/<name>/SKILL.md
//! ```
//!
//! Each SKILL.md has a YAML frontmatter block (the lines between the first
//! pair of `---` markers) carrying `name` and `description`, followed by a
//! markdown body:
//!
//! ```markdown
//! ---
//! name: my-skill
//! description: One-line summary
//! ---
//! <workflow body in markdown>
//! ```
//!
//! This module is **model-routed**: it only discovers skills and renders a
//! compact index for the system prompt. There is deliberately **no keyword
//! matching** here — that was a locked v1.0.0 decision in the JS reference
//! (`mcp-castor/src/skills.js`), whose matching / negation-regex code is
//! deleted by design. The model reads the SKILL.md body via `read_file` when
//! a skill is relevant; injection happens runner-side later.
//!
//! Frontmatter parsing is hand-rolled (a flat `key: value` subset) rather than
//! pulling in a YAML crate: for a two-field frontmatter the hand parser is
//! simpler, dependency-free, and byte-stable, so a mainstream frontmatter
//! crate is *not* warranted here.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// A discovered skill: its frontmatter identity plus the path to its
/// `SKILL.md` so the model can read the body on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Split a SKILL.md document into `(frontmatter, body)`.
///
/// The frontmatter is the block between the FIRST `---` line and the NEXT
/// `---` line. If the document does not open with `---` (or has no closing
/// `---`), the whole document is treated as the body with empty frontmatter.
fn split_frontmatter(text: &str) -> (String, String) {
    // Normalize line endings so the split is stable across CRLF/LF files.
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();

    // The first non-empty line must be exactly `---` to open frontmatter.
    let mut open_idx = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if line.trim() == "---" {
            open_idx = Some(i);
        }
        break; // only the first non-empty line can open it
    }

    let Some(open_idx) = open_idx else {
        return (String::new(), normalized);
    };

    let close_idx = lines
        .iter()
        .enumerate()
        .skip(open_idx + 1)
        .find(|(_, line)| line.trim() == "---")
        .map(|(i, _)| i);

    let Some(close_idx) = close_idx else {
        // Opened but never closed: treat the whole thing as body (malformed).
        return (String::new(), normalized);
    };

    let frontmatter = lines[open_idx + 1..close_idx].join("\n");
    let body = lines[close_idx + 1..].join("\n").trim().to_string();
    (frontmatter, body)
}

/// Parse a frontmatter block into a plain map. Only `key: value` lines are
/// honored; anything else is ignored. Values are trimmed and de-quoted.
fn parse_frontmatter(frontmatter: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if frontmatter.is_empty() {
        return out;
    }
    for line in frontmatter.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(colon) = trimmed.find(':') else {
            continue;
        };
        let key = trimmed[..colon].trim().to_string();
        let mut value = trimmed[colon + 1..].trim().to_string();
        // Strip a single pair of surrounding quotes.
        if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value = value[1..value.len() - 1].to_string();
        }
        if !key.is_empty() {
            out.insert(key, value);
        }
    }
    out
}

/// Load and parse every skill under each of the given directories.
///
/// Scans each dir for `*/SKILL.md`. Unreadable or malformed entries are
/// skipped silently. A missing directory is skipped silently (no error).
/// A skill whose frontmatter lacks a `description` is skipped. Duplicate
/// names: the first directory wins.
pub fn load_skills(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut out: Vec<Skill> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue, // missing dir: skipped silently
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_file = path.join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&skill_file) else {
                continue; // no SKILL.md in this subdirectory
            };

            let (frontmatter, _body) = split_frontmatter(&text);
            let fm = parse_frontmatter(&frontmatter);

            let dir_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let name = fm
                .get("name")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or(dir_name);
            let Some(description) = fm
                .get("description")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            else {
                // Missing (or empty) description: skill is skipped.
                continue;
            };

            if !seen.insert(name.clone()) {
                continue; // duplicate name: first dir wins
            }
            out.push(Skill {
                name,
                description,
                path: skill_file,
            });
        }
    }
    out
}

/// Render the system-prompt index block: one line per skill, in stable
/// (name-sorted) order, telling the model to read the SKILL.md body via
/// `read_file` when a skill is relevant.
pub fn render_index(skills: &[Skill]) -> String {
    let mut sorted: Vec<&Skill> = skills.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));

    let mut out = String::new();
    out.push_str(
        "Available skills. When a task matches a skill below, read its SKILL.md body with the read_file tool before acting.\n",
    );
    for s in sorted {
        out.push_str(&format!(
            "- {}: {} (read_file: {})\n",
            s.name,
            s.description,
            s.path.display()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A fresh, unique temp dir (created). Caller is responsible for cleanup.
    fn fresh_temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p =
            std::env::temp_dir().join(format!("castor_skills_test_{}_{}", std::process::id(), n));
        let _ = fs::create_dir_all(&p);
        p
    }

    /// Write `<root>/<name>/SKILL.md` with the given frontmatter + body.
    fn write_skill(root: &Path, name: &str, frontmatter: &str, body: &str) {
        let d = root.join(name);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("SKILL.md"), format!("---\n{frontmatter}---\n{body}")).unwrap();
    }

    #[test]
    fn index_has_one_line_per_skill_in_name_order() {
        let root = fresh_temp_dir();
        // Create 3 skills whose dir names are NOT in alphabetical order, to
        // prove the index sorts by name.
        write_skill(
            &root,
            "zeta",
            "name: zeta\ndescription: Z skill\n",
            "Z body\n",
        );
        write_skill(
            &root,
            "alpha",
            "name: alpha\ndescription: A skill\n",
            "A body\n",
        );
        write_skill(
            &root,
            "mid",
            "name: mid\ndescription: M skill\n",
            "M body\n",
        );

        let skills = load_skills(std::slice::from_ref(&root));
        assert_eq!(skills.len(), 3);

        let idx = render_index(&skills);
        let lines: Vec<&str> = idx.lines().collect();
        // One header line + one line per skill.
        assert_eq!(lines.len(), 4);
        // Skill lines in name order: alpha, mid, zeta.
        assert!(lines[1].contains("alpha"), "line 1: {}", lines[1]);
        assert!(lines[2].contains("mid"), "line 2: {}", lines[2]);
        assert!(lines[3].contains("zeta"), "line 3: {}", lines[3]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_description_is_skipped() {
        let root = fresh_temp_dir();
        write_skill(&root, "good", "name: good\ndescription: Fine\n", "body\n");
        // No description key at all.
        write_skill(&root, "nobody", "name: nobody\n", "body\n");
        // Empty description value.
        write_skill(
            &root,
            "emptydesc",
            "name: emptydesc\ndescription:\n",
            "body\n",
        );

        let skills = load_skills(std::slice::from_ref(&root));
        assert_eq!(skills.len(), 1, "only the well-formed skill should load");
        assert_eq!(skills[0].name, "good");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_and_missing_dirs_yield_empty_vec() {
        // Missing dir: skipped silently, no error.
        let missing = std::env::temp_dir().join("castor_skills_missing_dir_never_created");
        let skills = load_skills(&[missing]);
        assert!(skills.is_empty());

        // Existing but empty dir.
        let root = fresh_temp_dir();
        let skills = load_skills(std::slice::from_ref(&root));
        assert!(skills.is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_names_first_dir_wins() {
        let root1 = fresh_temp_dir();
        let root2 = fresh_temp_dir();
        write_skill(
            &root1,
            "dup",
            "name: dup\ndescription: from dir1\n",
            "body1\n",
        );
        write_skill(
            &root2,
            "dup",
            "name: dup\ndescription: from dir2\n",
            "body2\n",
        );

        let skills = load_skills(&[root1.clone(), root2.clone()]);
        assert_eq!(skills.len(), 1, "duplicate name should collapse to one");
        assert_eq!(skills[0].description, "from dir1", "first dir wins");
        let _ = fs::remove_dir_all(&root1);
        let _ = fs::remove_dir_all(&root2);
    }

    #[test]
    fn index_is_byte_stable_across_calls() {
        let root = fresh_temp_dir();
        write_skill(&root, "b", "name: b\ndescription: B\n", "body\n");
        write_skill(&root, "a", "name: a\ndescription: A\n", "body\n");
        let skills = load_skills(std::slice::from_ref(&root));

        let first = render_index(&skills);
        let second = render_index(&skills);
        assert_eq!(first, second);
        assert_eq!(first.as_bytes(), second.as_bytes());
        let _ = fs::remove_dir_all(&root);
    }
}
