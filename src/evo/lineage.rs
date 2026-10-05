//! AVO-inspired offline lineage: scored commits as DAG nodes with explicit
//! parent links.
//!
//! Design invariants (locked):
//! - The lineage is an **append-only JSONL file**: one [`LineageNode`] per
//!   line. Nodes are never overwritten — there is no last-writer-wins
//!   anywhere in this module.
//! - [`Lineage::add_node`] requires every parent to already exist, so
//!   **cycles are impossible by construction**: a node can only reference
//!   nodes that were appended earlier.
//! - [`Lineage::load`] is strict: a file with a duplicate id or a dangling
//!   parent reference is refused with a typed [`LineageError::Corruption`]
//!   naming the offending line. The file is never silently repaired.
//! - Parent selection is **greedy**: [`Lineage::best_parent`] picks the
//!   highest fitness, with ties broken by the earliest `created_at`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Identifier of a commit in the lineage DAG.
pub type CommitId = String;

/// One node in the lineage DAG: a scored commit with explicit parent links.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageNode {
    pub id: CommitId,
    pub parents: Vec<CommitId>,
    pub fitness: Option<f64>,
    pub artifact_ref: String,
    pub created_at: String,
}

/// Typed lineage errors.
#[derive(Debug, Error)]
pub enum LineageError {
    /// `add_node` referenced a parent that does not exist in the lineage.
    #[error("unknown parent '{0}' for node '{1}'")]
    UnknownParent(CommitId, CommitId),
    /// A node id appears more than once (in the file or on append).
    #[error("duplicate node id '{0}'")]
    DuplicateId(CommitId),
    /// The lineage file is corrupt; the message names the offending line.
    #[error("corrupt lineage at line {0}: {1}")]
    Corruption(usize, String),
    /// I/O failure reading or writing the lineage file.
    #[error("lineage io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization/deserialization failure.
    #[error("lineage json: {0}")]
    Json(#[from] serde_json::Error),
}

// Manual PartialEq: the Io and Json variants wrap types that do not
// implement PartialEq, so the derive cannot be used.
impl PartialEq for LineageError {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::UnknownParent(a, b), Self::UnknownParent(x, y)) => a == x && b == y,
            (Self::DuplicateId(a), Self::DuplicateId(b)) => a == b,
            (Self::Corruption(l, m), Self::Corruption(x, y)) => l == x && m == y,
            _ => false,
        }
    }
}

/// Append-only lineage DAG loaded from / saved to a JSONL file.
#[derive(Debug, Default)]
pub struct Lineage {
    nodes: Vec<LineageNode>,
    path: Option<PathBuf>,
}

impl Lineage {
    /// Loads the lineage from a JSONL file (one node per line).
    ///
    /// A missing file yields an empty lineage. A file with a duplicate id,
    /// a dangling parent reference, or a self-referencing parent is refused
    /// with [`LineageError::Corruption`] naming the offending line — the
    /// file is never silently repaired.
    pub fn load(path: &Path) -> Result<Self, LineageError> {
        let mut nodes: Vec<(usize, LineageNode)> = Vec::new();
        if path.exists() {
            let raw = fs::read_to_string(path)?;
            let mut seen: HashSet<CommitId> = HashSet::new();
            for (idx, line) in raw.lines().enumerate() {
                let line_no = idx + 1;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let node: LineageNode = serde_json::from_str(trimmed)
                    .map_err(|e| LineageError::Corruption(line_no, format!("invalid JSON: {e}")))?;
                if !seen.insert(node.id.clone()) {
                    return Err(LineageError::Corruption(
                        line_no,
                        format!("duplicate id '{}'", node.id),
                    ));
                }
                nodes.push((line_no, node));
            }
            // Dangling / self-referencing parent check: every parent must be
            // a distinct node present in the file.
            for (line_no, node) in &nodes {
                for parent in &node.parents {
                    if parent == &node.id {
                        return Err(LineageError::Corruption(
                            *line_no,
                            format!("self-referencing parent '{}'", node.id),
                        ));
                    }
                    if !seen.contains(parent) {
                        return Err(LineageError::Corruption(
                            *line_no,
                            format!("dangling parent '{}' for node '{}'", parent, node.id),
                        ));
                    }
                }
            }
        }
        Ok(Self {
            nodes: nodes.into_iter().map(|(_, n)| n).collect(),
            path: Some(path.to_path_buf()),
        })
    }

    /// Writes the lineage to its bound path as JSONL (one node per line).
    pub fn save(&self) -> Result<(), LineageError> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| LineageError::Corruption(0, "no path bound to this lineage".into()))?;
        self.save_to(&path)
    }

    /// Writes the lineage to an explicit path as JSONL (one node per line).
    pub fn save_to(&self, path: &Path) -> Result<(), LineageError> {
        let mut out = String::new();
        for node in &self.nodes {
            out.push_str(&serde_json::to_string(node)?);
            out.push('\n');
        }
        fs::write(path, out)?;
        Ok(())
    }

    /// Appends a node to the lineage.
    ///
    /// Every parent must already exist (typed [`LineageError::UnknownParent`]
    /// otherwise) and the id must be new ([`LineageError::DuplicateId`]
    /// otherwise). Cycles are impossible by construction: a node can only
    /// reference nodes that were appended earlier.
    pub fn add_node(&mut self, node: LineageNode) -> Result<(), LineageError> {
        let known: HashSet<&str> = self.nodes.iter().map(|n| n.id.as_str()).collect();
        if known.contains(node.id.as_str()) {
            return Err(LineageError::DuplicateId(node.id));
        }
        for parent in &node.parents {
            if !known.contains(parent.as_str()) {
                return Err(LineageError::UnknownParent(parent.clone(), node.id));
            }
        }
        self.nodes.push(node);
        Ok(())
    }

    /// Greedy best-parent selection: the node with the highest fitness;
    /// ties broken by the earliest `created_at`. Unscored nodes
    /// (`fitness == None`) are ineligible.
    pub fn best_parent(&self) -> Option<&LineageNode> {
        self.nodes
            .iter()
            .filter(|n| n.fitness.is_some())
            .max_by(|a, b| compare_fitness(a, b))
    }

    /// The best leaf: a leaf is a node that is not a parent of any other
    /// node. Among leaves, the same greedy rule as [`Self::best_parent`].
    pub fn head(&self) -> Option<&LineageNode> {
        let parents: HashSet<&str> = self
            .nodes
            .iter()
            .flat_map(|n| n.parents.iter())
            .map(String::as_str)
            .collect();
        self.nodes
            .iter()
            .filter(|n| !parents.contains(n.id.as_str()))
            .filter(|n| n.fitness.is_some())
            .max_by(|a, b| compare_fitness(a, b))
    }

    /// Walks the ancestor chain of `id`: the node itself first, then all
    /// transitive parents (deduplicated). Returns `None` if the id is
    /// unknown or any referenced parent is missing.
    pub fn walk_ancestors(&self, id: &str) -> Option<Vec<&LineageNode>> {
        let mut out: Vec<&LineageNode> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = vec![id];
        while let Some(cur) = stack.pop() {
            if !seen.insert(cur) {
                continue;
            }
            let node = self.nodes.iter().find(|n| n.id == cur)?;
            out.push(node);
            stack.extend(node.parents.iter().map(String::as_str));
        }
        Some(out)
    }

    /// All nodes in append order.
    pub fn nodes(&self) -> &[LineageNode] {
        &self.nodes
    }
}

/// Greedy comparison: higher fitness wins; on a fitness tie the earlier
/// `created_at` wins (ISO-8601 strings compare lexicographically).
fn compare_fitness(a: &LineageNode, b: &LineageNode) -> std::cmp::Ordering {
    match (a.fitness, b.fitness) {
        (Some(x), Some(y)) => x
            .partial_cmp(&y)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.created_at.cmp(&a.created_at)),
        _ => std::cmp::Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("castor_evo_lineage_tests");
        let _ = fs::create_dir_all(&dir);
        let p = dir.join(format!("{}_{}.jsonl", name, std::process::id()));
        let _ = fs::remove_file(&p);
        p
    }

    fn node(id: &str, parents: &[&str], fitness: Option<f64>, created: &str) -> LineageNode {
        LineageNode {
            id: id.into(),
            parents: parents.iter().map(|s| s.to_string()).collect(),
            fitness,
            artifact_ref: format!("artifact://{id}"),
            created_at: created.into(),
        }
    }

    #[test]
    fn best_parent_picks_fitter() {
        let mut lin = Lineage::default();
        lin.add_node(node("a", &[], Some(1.0), "2026-01-01T00:00:00Z"))
            .unwrap();
        lin.add_node(node("b", &["a"], Some(2.0), "2026-01-02T00:00:00Z"))
            .unwrap();
        assert_eq!(lin.best_parent().unwrap().id, "b");
    }

    #[test]
    fn best_parent_tie_broken_by_earliest() {
        let mut lin = Lineage::default();
        lin.add_node(node("a", &[], Some(1.0), "2026-01-02T00:00:00Z"))
            .unwrap();
        lin.add_node(node("b", &[], Some(1.0), "2026-01-01T00:00:00Z"))
            .unwrap();
        assert_eq!(lin.best_parent().unwrap().id, "b");
    }

    #[test]
    fn unknown_parent_refused() {
        let mut lin = Lineage::default();
        lin.add_node(node("a", &[], Some(1.0), "t1")).unwrap();
        let err = lin
            .add_node(node("b", &["ghost"], Some(2.0), "t2"))
            .unwrap_err();
        assert_eq!(err, LineageError::UnknownParent("ghost".into(), "b".into()));
    }

    #[test]
    fn duplicate_id_file_is_corruption_naming_line() {
        let p = tmp_path("dup");
        fs::write(
            &p,
            "{\"id\":\"a\",\"parents\":[],\"fitness\":1.0,\"artifact_ref\":\"x\",\"created_at\":\"t1\"}\n\
             {\"id\":\"a\",\"parents\":[],\"fitness\":2.0,\"artifact_ref\":\"x\",\"created_at\":\"t2\"}\n",
        )
        .unwrap();
        let err = Lineage::load(&p).unwrap_err();
        match &err {
            LineageError::Corruption(line, msg) => {
                assert_eq!(*line, 2, "must name the second line");
                assert!(msg.contains("a"), "{msg}");
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn dangling_parent_is_corruption_naming_line() {
        let p = tmp_path("dangling");
        fs::write(
            &p,
            "{\"id\":\"a\",\"parents\":[\"ghost\"],\"fitness\":1.0,\"artifact_ref\":\"x\",\"created_at\":\"t1\"}\n",
        )
        .unwrap();
        let err = Lineage::load(&p).unwrap_err();
        match &err {
            LineageError::Corruption(line, msg) => {
                assert_eq!(*line, 1, "must name the offending line");
                assert!(msg.contains("ghost"), "{msg}");
            }
            other => panic!("expected Corruption, got {other:?}"),
        }
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn empty_file_is_none_everywhere() {
        let p = tmp_path("empty");
        fs::write(&p, "").unwrap();
        let lin = Lineage::load(&p).unwrap();
        assert!(lin.best_parent().is_none());
        assert!(lin.head().is_none());
        assert!(lin.walk_ancestors("a").is_none());
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn head_is_best_leaf() {
        let mut lin = Lineage::default();
        lin.add_node(node("a", &[], Some(1.0), "2026-01-01T00:00:00Z"))
            .unwrap();
        lin.add_node(node("b", &["a"], Some(2.0), "2026-01-02T00:00:00Z"))
            .unwrap();
        lin.add_node(node("c", &["a"], Some(3.0), "2026-01-03T00:00:00Z"))
            .unwrap();
        // 'a' is not a leaf (parent of b and c); the best leaf is 'c'.
        assert_eq!(lin.head().unwrap().id, "c");
        assert_eq!(lin.best_parent().unwrap().id, "c");
    }

    #[test]
    fn save_load_roundtrip() {
        let p = tmp_path("roundtrip");
        let mut lin = Lineage::default();
        lin.add_node(node("a", &[], Some(1.0), "2026-01-01T00:00:00Z"))
            .unwrap();
        lin.add_node(node("b", &["a"], Some(2.0), "2026-01-02T00:00:00Z"))
            .unwrap();
        lin.save_to(&p).unwrap();
        let loaded = Lineage::load(&p).unwrap();
        assert_eq!(loaded.nodes().len(), 2);
        assert_eq!(loaded.best_parent().unwrap().id, "b");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn walk_ancestors_returns_chain() {
        let mut lin = Lineage::default();
        lin.add_node(node("a", &[], Some(1.0), "t1")).unwrap();
        lin.add_node(node("b", &["a"], Some(2.0), "t2")).unwrap();
        let chain = lin.walk_ancestors("b").unwrap();
        assert_eq!(
            chain.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }
}
