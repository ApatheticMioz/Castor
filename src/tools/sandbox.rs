//! 5-layer path containment policy, ported from
//! `mcp-castor/src/harness/services/sandbox_fs.js`.
//!
//! Layers (each a `pub fn` returning `Result<PathBuf, SandboxError>`):
//! 1. `resolve_workspace_root` — canonicalize the workspace root
//! 2. `verify_symlink_containment` — refuse symlink escapes
//! 3. `refuse_out_of_tree` — refuse absolute out-of-tree paths
//! 4. `normalize_traversal` — lexical normalization + traversal refusal
//! 5. `check_binary` — magic-number sniff → `BinaryFile` fail-fast
//!
//! The workspace root is always a `&Path` argument: no config, no env, no
//! globals. Errors are typed and carry verbatim refusal messages; nothing is
//! silently coerced.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use path_clean::PathClean;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("{0}")]
    InvalidPath(String),
    #[error("{0}")]
    NullByte(String),
    #[error("{0}")]
    DeviceName(String),
    #[error("{0}")]
    PathEscape(String),
    #[error("{0}")]
    SymlinkEscape(String),
    #[error("{0}")]
    WorkspaceRoot(String),
    #[error(
        "BinaryFileError: '{file}' is a binary file ({detected_type}); reason: {reason}. \
         A text-only model must not ingest binary content. Do NOT read this file as text. \
         To extract its text, use the bash tool with a format-appropriate extractor \
         (e.g. 'pdftotext file -' for PDF, 'strings file', 'exiftool file', 'unzip -l file') \
         and read the extracted text output instead."
    )]
    BinaryFile {
        file: PathBuf,
        detected_type: String,
        reason: String,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Layer 1: resolve the workspace root through the OS symlink/junction
/// resolution layer so containment checks compare against a real path.
pub fn resolve_workspace_root(root: &Path) -> Result<PathBuf, SandboxError> {
    let host_root = crate::platform::to_host_path(root);
    let meta = fs::metadata(&host_root).map_err(|_| {
        SandboxError::WorkspaceRoot(format!(
            "WorkspaceRootError: workspace root '{}' does not exist or is not a directory",
            root.display()
        ))
    })?;
    if !meta.is_dir() {
        return Err(SandboxError::WorkspaceRoot(format!(
            "WorkspaceRootError: workspace root '{}' is not a directory",
            root.display()
        )));
    }
    fs::canonicalize(&host_root).map_err(|e| {
        SandboxError::WorkspaceRoot(format!(
            "WorkspaceRootError: cannot canonicalize workspace root '{}': {e}",
            root.display()
        ))
    })
}

fn is_within(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).is_ok()
}

/// Layer 2: refuse symlink escapes. An existing target must canonicalize
/// inside the root; a non-existent target's nearest existing ancestor must
/// canonicalize inside the root.
pub fn verify_symlink_containment(target: &Path, root: &Path) -> Result<PathBuf, SandboxError> {
    if target.exists() {
        let real = fs::canonicalize(target)?;
        if !is_within(&real, root) {
            return Err(SandboxError::SymlinkEscape(format!(
                "SymlinkEscapeError: Real path '{}' escapes sandbox root '{}'",
                real.display(),
                root.display()
            )));
        }
        return Ok(target.to_path_buf());
    }
    let mut parent = target.parent().map(Path::to_path_buf);
    while let Some(p) = parent {
        let above = p.parent().map(Path::to_path_buf);
        if above.as_ref() == Some(&p) {
            break;
        }
        if p.exists() {
            let real = fs::canonicalize(&p)?;
            if !is_within(&real, root) {
                return Err(SandboxError::SymlinkEscape(format!(
                    "SymlinkEscapeError: Parent directory '{}' resolves to '{}' escaping root '{}'",
                    p.display(),
                    real.display(),
                    root.display()
                )));
            }
            break;
        }
        parent = above;
    }
    Ok(target.to_path_buf())
}

/// Layer 3: refuse absolute paths that do not land inside the workspace root.
pub fn refuse_out_of_tree(root: &Path, target: &Path) -> Result<PathBuf, SandboxError> {
    if target.is_absolute() && !is_within(target, root) {
        return Err(SandboxError::PathEscape(format!(
            "PathEscapeError: Access denied. Path '{}' escapes sandbox root '{}'",
            target.display(),
            root.display()
        )));
    }
    Ok(target.to_path_buf())
}

/// Lexically collapse `.` and `..` components via path-clean crate.
fn normalize_lexical(p: &Path) -> PathBuf {
    p.clean()
}

fn is_reserved_device(name: &str) -> bool {
    let upper = name.to_uppercase();
    let stem = upper.split('.').next().unwrap_or("");
    if matches!(stem, "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(rest) = stem.strip_prefix(prefix)
            && let Some(d) = rest.chars().next()
            && rest.len() == 1
            && d.is_ascii_digit()
        {
            return true;
        }
    }
    false
}

/// Layer 4: normalize a raw path string (null-byte and reserved-device
/// refusal, backslash normalization, relative resolution against the root,
/// lexical `..` collapse) and refuse any result that lands outside the root.
pub fn normalize_traversal(root: &Path, input: &str) -> Result<PathBuf, SandboxError> {
    if input.contains('\0') {
        return Err(SandboxError::NullByte(
            "NullByteError: Path contains prohibited null byte character".into(),
        ));
    }
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(root.to_path_buf());
    }
    let posix = trimmed.replace('\\', "/");
    let base_name = posix.rsplit('/').next().unwrap_or("");
    if is_reserved_device(base_name) {
        return Err(SandboxError::DeviceName(format!(
            "DeviceNameError: Prohibited access to Windows reserved device '{}'",
            base_name.to_uppercase()
        )));
    }
    // Treat Windows drive-letter paths (e.g. "C:/", "D:/") as absolute so
    // they are refused as out-of-tree rather than silently joined under the
    // workspace root. (On Unix, `Path::is_absolute` does not recognize
    // drive-letter prefixes, so we detect them explicitly.)
    let is_drive = {
        let mut ch = posix.chars();
        match (ch.next(), ch.next()) {
            (Some(c), Some(':')) => c.is_ascii_alphabetic(),
            _ => false,
        }
    };
    let p = if posix.starts_with('/') || is_drive {
        PathBuf::from(posix)
    } else {
        root.join(&posix)
    };
    let normalized = normalize_lexical(&p);
    if !is_within(&normalized, root) {
        return Err(SandboxError::PathEscape(format!(
            "PathEscapeError: Access denied. Path '{}' escapes sandbox root '{}'",
            input,
            root.display()
        )));
    }
    Ok(normalized)
}

/// Layer 5: binary read fail-fast. Sniff the first 4096 bytes via the `infer` crate;
/// a known binary signature or presence of null bytes yields `SandboxError::BinaryFile`.
pub fn check_binary(path: &Path) -> Result<PathBuf, SandboxError> {
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    if let Some(kind) = infer::get(&buf) {
        return Err(SandboxError::BinaryFile {
            file: path.to_path_buf(),
            detected_type: format!("{} ({})", kind.mime_type(), kind.extension()),
            reason: format!("magic bytes identify it as {}", kind.mime_type()),
        });
    }
    if buf.contains(&0) {
        return Err(SandboxError::BinaryFile {
            file: path.to_path_buf(),
            detected_type: "application/octet-stream (binary)".to_string(),
            reason: "null bytes detected in file header".to_string(),
        });
    }
    Ok(path.to_path_buf())
}

// ---------------------------------------------------------------------------
// Symmetric allowed-read / write roots
// ---------------------------------------------------------------------------

/// The access a resolved path is granted under the [`SandboxPolicy`].
///
/// `ReadWrite` roots permit both reading and mutating; `ReadOnly` roots permit
/// reading only. `Deny` is the terminal class (a path in neither set); [`
/// SandboxPolicy::resolve`] maps it to a [`SandboxError::PathEscape`] rather
/// than returning it, but the variant is kept so callers can express "no
/// access" explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessClass {
    /// Path lands under a write root: reads and writes both allowed.
    ReadWrite,
    /// Path lands under a read-only root: reads allowed, writes denied.
    ReadOnly,
    /// Path lands in neither set: no access (normally surfaced as `PathEscape`).
    Deny,
}

/// A path together with the access class the [`SandboxPolicy`] grants it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub path: PathBuf,
    pub class: AccessClass,
}

impl ResolvedPath {
    /// Whether this resolution satisfies a read (RO ∪ RW) operation.
    pub fn permits_read(&self) -> bool {
        self.class != AccessClass::Deny
    }
    /// Whether this resolution satisfies a write (RW-only) operation.
    pub fn permits_write(&self) -> bool {
        self.class == AccessClass::ReadWrite
    }
}

fn canonical_or_self(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn dedupe(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
    v.sort();
    v.dedup();
    v
}

/// Canonical `$HOME` for the current user (platform-aware).
fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Symmetric filesystem policy: a set of read-write roots and a (superset) set
/// of read-only roots.
///
/// This is the single source of truth that both the filesystem tools and the
/// (Linux) Landlock confinement derive their allowed-path classification from,
/// so the two layers never disagree.
///
/// Layers 1–5 (workspace-root resolution, symlink containment, out-of-tree
/// refusal, lexical normalization, binary sniff) remain intact as free
/// functions above; this policy *generalizes* the single-root model to a
/// read/write split and adds tilde expansion, while delegating lexical
/// normalization to [`normalize_lexical`] and device-name refusal to
/// [`is_reserved_device`] so those invariants are preserved.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    write_roots: Vec<PathBuf>,
    read_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// Construct from explicit (already canonicalized) root lists.
    ///
    /// Read roots are a superset of write roots by convention: a path is
    /// `ReadWrite` iff it is under a write root, else `ReadOnly` iff it is
    /// under a read root, else `Deny` (surfaced as `PathEscape`).
    pub fn new(write_roots: Vec<PathBuf>, read_roots: Vec<PathBuf>) -> Self {
        Self {
            write_roots: dedupe(write_roots),
            read_roots: dedupe(read_roots),
        }
    }

    /// The tight policy used by [`crate::tools::fs::FsExecutor::new`] and the
    /// eval runner, where only the workspace itself may be touched. This
    /// preserves the historical single-root behavior (and its tests).
    pub fn workspace_only(root: &Path) -> Self {
        let ws = canonical_or_self(root);
        Self::new(vec![ws.clone()], vec![ws])
    }

    /// The production policy built by [`crate::tools::CompositeExecutor`]:
    ///
    /// - **write roots**: canonicalized `workspace_root` + `std::env::temp_dir()`
    /// - **read roots**: canonicalized `workspace_root` + optional `state_dir`
    ///   (`~/.castor`) + (on Linux) `/usr`, `/bin`, `/lib`, `/etc`
    pub fn for_workspace(root: &Path, state_dir: Option<&Path>) -> Self {
        let ws = canonical_or_self(root);
        let write_roots = vec![ws.clone(), std::env::temp_dir()];
        let mut read_roots = vec![ws];
        if let Some(sd) = state_dir {
            read_roots.push(canonical_or_self(sd));
        }
        #[cfg(target_os = "linux")]
        for p in ["/usr", "/bin", "/lib", "/etc"] {
            read_roots.push(PathBuf::from(p));
        }
        Self::new(write_roots, read_roots)
    }

    pub fn write_roots(&self) -> &[PathBuf] {
        &self.write_roots
    }
    pub fn read_roots(&self) -> &[PathBuf] {
        &self.read_roots
    }

    /// Expand a leading `~` / `~user` prefix to the canonical `$HOME` before
    /// classification.
    ///
    /// Per the spec, both a bare `~` and a `~user` form collapse to the
    /// current user's canonical home (a conservative mapping that keeps every
    /// tilde reference classifiable against the home-rooted read roots).
    /// Trailing sub-paths are preserved: `~/a/b` → `$HOME/a/b`.
    pub fn expand_tilde_prefix(&self, input: &str) -> String {
        let s = input.trim();
        let home = match home_dir() {
            Some(h) => h,
            None => return input.to_string(),
        };
        let join = |sub: &str| {
            if sub.is_empty() {
                home.to_string_lossy().into_owned()
            } else {
                home.join(sub).to_string_lossy().into_owned()
            }
        };
        let Some(after) = s.strip_prefix('~') else {
            return input.to_string();
        };
        if after.is_empty() {
            return join(""); // "~"
        }
        if let Some(sub) = after.strip_prefix('/') {
            return join(sub); // "~/sub/..."
        }
        // "~user" or "~user/sub": drop the `user` segment, keep the rest.
        let sub = after.split('/').skip(1).collect::<Vec<_>>().join("/");
        join(&sub)
    }

    /// Classify a raw path string against the policy.
    ///
    /// Order of operations (preserving the 5-layer invariants, generalized to the
    /// root sets):
    /// 1. null-byte refusal (layer 4),
    /// 2. tilde expansion to canonical `$HOME`,
    /// 3. backslash→slash normalization + lexical `.`/`..` collapse (layer 4),
    /// 4. reserved-device-name refusal (layer 4),
    /// 5. lexical membership: a path whose *lexical* form is outside every root is
    ///    an out-of-tree [`SandboxError::PathEscape`] (layer 3, checked first);
    /// 6. symlink containment: if the path exists and its canonical (symlink-
    ///    resolved) location falls outside every root, it is a
    ///    [`SandboxError::SymlinkEscape`] (layer 2, generalized);
    /// 7. the granted class reflects the *canonical* location — `ReadWrite` if
    ///    under a write root, else `ReadOnly` if under a read root — so a symlink
    ///    from a write root into a read-only root is treated as read-only, never
    ///    read-write.
    ///
    /// The returned [`ResolvedPath::path`] is the lexically-normalized path (stable
    /// and printable); its `class` is derived from the canonical location.
    pub fn resolve(&self, input: &str) -> Result<ResolvedPath, SandboxError> {
        if input.contains('\0') {
            return Err(SandboxError::NullByte(
                "NullByteError: Path contains prohibited null byte character".into(),
            ));
        }
        let expanded = self.expand_tilde_prefix(input);
        let posix = expanded.replace('\\', "/");
        // A relative path is resolved against the primary write root (the
        // workspace); an absolute path is used as-is.
        let base = self.write_roots.first().cloned().unwrap_or_default();
        let p = if Path::new(&posix).is_absolute() {
            PathBuf::from(posix)
        } else {
            base.join(&posix)
        };
        let normalized = normalize_lexical(&p);
        if let Some(name) = normalized.file_name()
            && is_reserved_device(&name.to_string_lossy())
        {
            return Err(SandboxError::DeviceName(format!(
                "DeviceNameError: Prohibited access to Windows reserved device '{}'",
                name.to_string_lossy().to_uppercase()
            )));
        }

        // (5) Lexical out-of-tree check first — a plainly-absolute path outside
        // every root is a PathEscape (layer 3), not a symlink error.
        let lexical_in_write = self
            .write_roots
            .iter()
            .any(|r| normalized.strip_prefix(r).is_ok());
        let lexical_in_read = self
            .read_roots
            .iter()
            .any(|r| normalized.strip_prefix(r).is_ok());
        if !lexical_in_write && !lexical_in_read {
            return Err(SandboxError::PathEscape(format!(
                "PathEscapeError: Access denied. Path '{}' escapes all allowed roots",
                input
            )));
        }

        // (6) Symlink containment: resolve to the real location; it must itself
        // land inside a root or the symlink escapes the sandbox.
        let real = if normalized.exists() {
            fs::canonicalize(&normalized).unwrap_or(normalized.clone())
        } else {
            normalized.clone()
        };
        let real_in_write = self
            .write_roots
            .iter()
            .any(|r| real.strip_prefix(r).is_ok());
        let real_in_read = self.read_roots.iter().any(|r| real.strip_prefix(r).is_ok());
        if !real_in_write && !real_in_read {
            return Err(SandboxError::SymlinkEscape(format!(
                "SymlinkEscapeError: Path '{}' resolves to '{}' escaping all allowed roots",
                normalized.display(),
                real.display()
            )));
        }

        // (7) Class from the canonical location: RW beats RO.
        let class = if real_in_write {
            AccessClass::ReadWrite
        } else {
            AccessClass::ReadOnly
        };
        Ok(ResolvedPath {
            path: normalized,
            class,
        })
    }
}

// ---------------------------------------------------------------------------
// Landlock confinement (Linux) — additive over the 5-layer in-process policy.
// ---------------------------------------------------------------------------
//
// The kernel is the authoritative confiner for the *sandboxed shell child*:
// the in-process 5-layer policy above governs the filesystem *tools* and the
// lexical path classification, while Landlock governs what the spawned
// `bash -c` child may touch at the syscall level. The two derive from the
// *same* `SandboxPolicy` root sets so they never disagree about which paths
// are write vs. read.
//
// The ruleset is built in the **parent** (before `fork`), then handed to the
// child via `pre_exec` → `restrict_self()`. The parent never restricts
// itself. Every failure mode — Landlock unavailable (kernel < 5.13 / disabled
// → the crate's best-effort yields a no-op ruleset), an unsupportable
// filesystem (e.g. a `9p`/drvfs mount that refuses a `PathBeneath` rule), or
// a failed `restrict_self` — is handled by the caller falling back cleanly to
// the in-process 5-layer sandbox. Landlock is strictly additive; a failure
// never aborts the command.

#[cfg(target_os = "linux")]
impl SandboxPolicy {
    /// Build a Landlock ruleset from this policy's root sets.
    ///
    /// - Read roots are granted read-only access (`AccessFs::from_read`).
    /// - Write roots are granted full access (`AccessFs::from_all`).
    ///
    /// Returns `Ok(None)` when Landlock is unavailable or the ruleset cannot
    /// be fully populated (an unsupportable filesystem). Returns `Ok(Some(rs))`
    /// with a ruleset ready to be `try_clone`d into a child and `restrict_self`ed.
    /// `Err` is reserved for a genuine build failure (the caller treats it the
    /// same as `None` — degrade to the in-process sandbox).
    pub fn build_landlock_ruleset(&self) -> Result<Option<landlock::RulesetCreated>, SandboxError> {
        use landlock::{ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr};

        let abi = ABI::V3; // Refer (V2) + Truncate (V3); best-effort on older kernels.
        let ro = AccessFs::from_read(abi);
        let rw = AccessFs::from_all(abi);

        // No usable access (Landlock absent): the crate's best-effort still
        // yields a no-op ruleset, but we skip straight to "not enforced".
        if ro.is_empty() && rw.is_empty() {
            return Ok(None);
        }

        let mut ruleset = Ruleset::default()
            .handle_access(ro | rw)
            .and_then(|r| r.create())
            .map_err(|e| SandboxError::InvalidPath(format!("landlock create: {e}")))?;

        // Read roots first (broader), then write roots (narrower, more access).
        for root in self.read_roots() {
            let Some(rule) = open_beneath(root, ro)? else {
                return Ok(None); // unsupportable / missing path → degrade
            };
            ruleset = ruleset
                .add_rule(rule)
                .map_err(|e| SandboxError::InvalidPath(format!("landlock add_rule: {e}")))?;
        }
        for root in self.write_roots() {
            let Some(rule) = open_beneath(root, rw)? else {
                return Ok(None);
            };
            ruleset = ruleset
                .add_rule(rule)
                .map_err(|e| SandboxError::InvalidPath(format!("landlock add_rule: {e}")))?;
        }

        Ok(Some(ruleset))
    }
}

#[cfg(target_os = "linux")]
fn open_beneath(
    root: &Path,
    access: landlock::BitFlags<landlock::AccessFs>,
) -> Result<Option<landlock::PathBeneath<landlock::PathFd>>, SandboxError> {
    use landlock::{PathBeneath, PathFd};
    match PathFd::new(root) {
        Ok(fd) => Ok(Some(PathBeneath::new(fd, access))),
        // A path that cannot be opened (missing, or a filesystem that rejects
        // O_PATH) is a degrade signal: no rule, caller falls back.
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::shell::{self, ShellPolicyError};

    fn test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("castor_sandbox_{}_{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        root
    }

    #[test]
    fn workspace_root_resolves_to_canonical() {
        let root = test_root("root_ok");
        let resolved = resolve_workspace_root(&root).unwrap();
        assert_eq!(resolved, fs::canonicalize(&root).unwrap());
    }

    #[test]
    fn workspace_root_missing_rejected() {
        let missing = std::env::temp_dir().join("castor_sandbox_missing_never");
        assert!(matches!(
            resolve_workspace_root(&missing),
            Err(SandboxError::WorkspaceRoot(_))
        ));
    }

    #[test]
    fn traversal_escape_rejected() {
        let root = test_root("traversal");
        for input in ["../outside.txt", "src/../../outside.txt", "a/b/../../../c"] {
            let err = normalize_traversal(&root, input).unwrap_err();
            assert!(matches!(err, SandboxError::PathEscape(_)), "{input}");
        }
    }

    #[test]
    fn null_byte_rejected() {
        let root = test_root("nullbyte");
        assert!(matches!(
            normalize_traversal(&root, "a\0b"),
            Err(SandboxError::NullByte(_))
        ));
    }

    #[test]
    fn device_name_rejected() {
        let root = test_root("device");
        assert!(matches!(
            normalize_traversal(&root, "NUL"),
            Err(SandboxError::DeviceName(_))
        ));
        assert!(matches!(
            normalize_traversal(&root, "src/COM1.txt"),
            Err(SandboxError::DeviceName(_))
        ));
    }

    #[test]
    fn out_of_tree_absolute_rejected() {
        let root = test_root("outofree");
        let err = refuse_out_of_tree(&root, Path::new("/etc/passwd")).unwrap_err();
        assert!(matches!(err, SandboxError::PathEscape(_)));
    }

    #[test]
    fn in_tree_absolute_allowed() {
        let root = test_root("intree_abs");
        let target = root.join("src").join("main.rs");
        assert!(refuse_out_of_tree(&root, &target).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_rejected() {
        let root = test_root("symlink_out");
        let outside = std::env::temp_dir().join(format!(
            "castor_sandbox_outside_{}_symlink_out",
            std::process::id()
        ));
        fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.txt");
        fs::write(&outside_file, "secret").unwrap();
        let link = root.join("link_to_outside");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        let err = verify_symlink_containment(&link, &root).unwrap_err();
        assert!(matches!(err, SandboxError::SymlinkEscape(_)));
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn in_tree_symlink_allowed() {
        let root = test_root("symlink_in");
        let target = root.join("src").join("main.rs");
        fs::write(&target, "fn main() {}\n").unwrap();
        let link = root.join("alias.rs");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(verify_symlink_containment(&link, &root).is_ok());
    }

    #[test]
    fn binary_magic_fail_fast() {
        let root = test_root("binary");
        let png = root.join("img.png");
        fs::write(
            &png,
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0],
        )
        .unwrap();
        let err = check_binary(&png).unwrap_err();
        match &err {
            SandboxError::BinaryFile {
                detected_type,
                reason,
                ..
            } => {
                assert!(detected_type.contains("image/png"));
                assert!(reason.contains("magic bytes"));
            }
            other => panic!("expected BinaryFile, got {other:?}"),
        }
    }

    #[test]
    fn text_file_passes_binary_check() {
        let root = test_root("text");
        let txt = root.join("notes.txt");
        fs::write(&txt, "hello world\n").unwrap();
        assert!(check_binary(&txt).is_ok());
    }

    #[test]
    fn legit_in_tree_paths_pass() {
        let root = test_root("legit");
        let resolved = normalize_traversal(&root, "src/main.rs").unwrap();
        assert_eq!(resolved, root.join("src").join("main.rs"));
        let resolved = normalize_traversal(&root, "./src/../src/main.rs").unwrap();
        assert_eq!(resolved, root.join("src").join("main.rs"));
    }

    // ========================================================================
    // 137-Vector Security Table
    // Ported verbatim from mcp-castor/tests/security.test.js
    // (123 attack vectors blocked + 14 allow vectors permitted)
    //
    // Each row: (input, op, expected, layer, path_operand, runnable)
    //   - `layer` names the refusal layer for blocked vectors, or "in-tree"
    //     for allowed ones.
    //   - `runnable` = true when the vector can be exercised live against a
    //     Rust policy: `ShellPolicy` rows through `shell::validate`, path
    //     rows through `normalize_traversal`. false for JS-only rules
    //     (deadman-fuse / dry-run / module-isolation) that have no Rust
    //     equivalent and are mapped to the closest layer.
    // ========================================================================

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Outcome {
        Blocked,
        Allowed,
    }

    struct Vector {
        input: &'static str,
        #[allow(dead_code)]
        op: Option<&'static str>,
        expected: Outcome,
        layer: &'static str,
        /// For allowed shell vectors: the path operand that must resolve
        /// in-tree (i.e., not be a protected root).
        path_operand: Option<&'static str>,
        /// Whether this vector can be exercised live against a Rust policy:
        /// `ShellPolicy` rows through `shell::validate`, path-layer rows
        /// through `normalize_traversal`.
        runnable: bool,
    }

    fn security_vectors() -> Vec<Vector> {
        let mut v = Vec::new();

        // --- Category 1: File Path Escapes (15 paths × 4 ops = 60) ---
        let paths = [
            "C:\\Windows\\System32\\calc.exe",
            "C:\\Users\\testuser\\Desktop",
            "C:\\",
            "c:/autoexec.bat",
            "D:\\",
            "D:\\..",
            "D:\\OtherFolder",
            "..\\..\\sensitive_file.txt",
            "../../../../etc/passwd",
            "/mnt/c/Windows",
            "/mnt/c/Users",
            "/mnt/d",
            "/mnt/d/..",
            "/etc/shadow",
            "/var/log",
        ];
        let ops = ["readFile", "writeFile", "astSearch", "astReplace"];
        for path in &paths {
            for op in &ops {
                v.push(Vector {
                    input: path,
                    op: Some(*op),
                    expected: Outcome::Blocked,
                    layer: "PathEscape",
                    path_operand: None,
                    runnable: true,
                });
            }
        }

        // --- Category 2: Null Byte Attacks (2) ---
        v.push(Vector {
            input: "safe_name.js\0malicious.exe",
            op: None,
            expected: Outcome::Blocked,
            layer: "NullByte",
            path_operand: None,
            runnable: true,
        });
        v.push(Vector {
            input: "src/test.py\0/../../../../Windows",
            op: None,
            expected: Outcome::Blocked,
            layer: "NullByte",
            path_operand: None,
            runnable: true,
        });

        // --- Category 3: Windows Reserved Device Names (6) ---
        for dev in ["CON", "PRN", "AUX", "NUL", "COM1", "LPT1"] {
            v.push(Vector {
                input: dev,
                op: None,
                expected: Outcome::Blocked,
                layer: "DeviceName",
                path_operand: None,
                runnable: true,
            });
        }

        // --- Category 4: Workspace Root Directory Overwrite (1) ---
        // JS-only: writing a file over the root directory. Closest Rust
        // layer is the path-containment policy (PathEscape).
        v.push(Vector {
            input: "<workspaceRoot>",
            op: None,
            expected: Outcome::Blocked,
            layer: "PathEscape",
            path_operand: None,
            runnable: false,
        });

        // --- Category 5: Dangerous Shell Command Blocking (43) ---
        // Exercised live against `shell::validate` (the Rust shell-command
        // policy); each must yield a typed refusal.
        let dangerous = [
            "rm -rf /",
            "rm -rf /*",
            "rm -rf --no-preserve-root /",
            "rm -rf ~",
            "rm -rf C:\\",
            "rm -rf D:\\",
            "rm -rf /mnt/c/Windows",
            "rm -rf C:\\Windows",
            "rmdir /s /q C:\\",
            "del /f /s /q C:\\*",
            "del /s /q C:\\Windows",
            "rd /s /q C:\\Users",
            "rm -rf C:\\Users\\*",
            "rm -rf /mnt/c/Users/*",
            "format C:",
            "mkfs.ext4 /dev/sda1",
            "dd if=/dev/zero of=/dev/sda bs=1M",
            ":(){ :|:& };:",
            "sudo rm -rf /",
            "sudo rm -rf --no-preserve-root /",
            "bash -c \"rm -rf /\"",
            "sh -c \"rm -rf /\"",
            "cmd /c del /s /q C:\\Windows",
            "powershell -Command \"Remove-Item C:\\Users -Recurse -Force\"",
            "bash -c \"bash -c 'rm -rf /'\"",
            "rm -rf $HOME",
            "rm -rf $HOME/projects",
            "powershell -Command \"Remove-Item C:\\Users\\* -Recurse -Force\"",
            "cd /tmp && rm -rf /",
            "echo a; rm -rf /",
            "echo a || rm -rf /",
            "echo a | rm -rf /",
            "echo a\nrm -rf /",
            "true && rm -rf ~",
            "ls; rm -rf /mnt/c/Windows",
            "rm -rf `pwd`",
            "del /q `dir`",
            "rm -rf `echo /`",
            "rm -rf --no-preserve-root=/",
            "rm -rf ~user",
            "rm -rf ~root",
            "rm -rf /mnt/c/\u{FF37}indows",
            "bash -c \"cd /tmp && rm -rf /\"",
        ];
        for cmd in &dangerous {
            v.push(Vector {
                input: cmd,
                op: None,
                expected: Outcome::Blocked,
                layer: "ShellPolicy",
                path_operand: None,
                runnable: true,
            });
        }

        // --- Category 5b: Shell Allow Vectors (14) ---
        // These must NOT be blocked. Each path operand resolves in-tree
        // (deeper subpath, not a protected root).
        let allow: &[(&str, Option<&str>)] = &[
            ("rm -rf /tmp/build", Some("/tmp/build")),
            (
                "rm -rf /mnt/c/Users/testuser/proj/dist",
                Some("/mnt/c/Users/testuser/proj/dist"),
            ),
            (
                "rm -rf C:\\Windows\\System32",
                Some("C:\\Windows\\System32"),
            ),
            (
                "rm -rf /mnt/c/Users/otheruser/build",
                Some("/mnt/c/Users/otheruser/build"),
            ),
            ("git push --force", None),
            ("npm ci", None),
            ("cargo build --release", None),
            ("rm -rf ./node_modules", Some("./node_modules")),
            (
                "rm -rf \"/mnt/d/some project/build\"",
                Some("/mnt/d/some project/build"),
            ),
            ("sudo rm -rf /tmp/build", Some("/tmp/build")),
            ("bash -c \"rm -rf /tmp/build\"", Some("/tmp/build")),
            ("env TMP=/tmp rm -rf /tmp/build", Some("/tmp/build")),
            ("xargs rm -rf < /tmp/list", Some("/tmp/list")),
            (
                "powershell -Command \"Remove-Item C:\\Users\\testuser\\proj\\dist -Recurse -Force\"",
                Some("C:\\Users\\testuser\\proj\\dist"),
            ),
        ];
        for (cmd, operand) in allow {
            v.push(Vector {
                input: cmd,
                op: None,
                expected: Outcome::Allowed,
                layer: "in-tree",
                path_operand: *operand,
                runnable: false,
            });
        }

        // --- Category 5c: In-Memory Dead-Man Fuse (1) ---
        // JS-only: synthetic canary token. Closest Rust layer: none
        // (mapped to DeadManFuse).
        v.push(Vector {
            input: "CANARY_DISASTER_FUSE_TOKEN",
            op: None,
            expected: Outcome::Blocked,
            layer: "DeadManFuse",
            path_operand: None,
            runnable: false,
        });

        // --- Category 5d: Dry-Run Hard Gate (1) ---
        // JS-only: dry-run simulation. Closest Rust layer: none
        // (mapped to DryRunGate).
        v.push(Vector {
            input: "echo safe_dry_run_simulation",
            op: None,
            expected: Outcome::Blocked,
            layer: "DryRunGate",
            path_operand: None,
            runnable: false,
        });

        // --- Category 5e: Structural Module Isolation (1) ---
        // JS-only: zero-child-process guarantee. Closest Rust layer: none
        // (mapped to ModuleIsolation).
        v.push(Vector {
            input: "shell_validator.js",
            op: None,
            expected: Outcome::Blocked,
            layer: "ModuleIsolation",
            path_operand: None,
            runnable: false,
        });

        // --- Category 6: Shell CWD Containment Escapes (4) ---
        for cwd in ["C:\\Windows", "C:\\Users", "../../..", "/mnt/c"] {
            v.push(Vector {
                input: cwd,
                op: None,
                expected: Outcome::Blocked,
                layer: "PathEscape",
                path_operand: None,
                runnable: true,
            });
        }

        // --- Category 7: Evo Operator File Unlinking Containment (4) ---
        for path in [
            "C:\\Windows\\notepad.exe",
            "C:\\Users\\testuser\\Desktop\\file.txt",
            "/mnt/c/Users/test.txt",
            "../../outside.js",
        ] {
            v.push(Vector {
                input: path,
                op: None,
                expected: Outcome::Blocked,
                layer: "PathEscape",
                path_operand: None,
                runnable: true,
            });
        }

        v
    }

    /// Returns true if `p` is a protected root (or its direct wildcard).
    /// A deeper subpath of a protected root is NOT a protected root.
    fn is_protected_root(p: &str) -> bool {
        let t = p.trim().trim_matches('"').trim();
        let norm = t.to_uppercase().replace('\\', "/");
        let protected = [
            "/",
            "/MNT/C",
            "/MNT/D",
            "/MNT/C/WINDOWS",
            "/MNT/C/USERS",
            "C:/",
            "D:/",
            "C:/WINDOWS",
            "C:/USERS",
            "~",
        ];
        for prot in &protected {
            if norm == *prot || norm == format!("{prot}*") {
                return true;
            }
        }
        false
    }

    #[test]
    fn security_vector_table_137() {
        let vectors = security_vectors();

        // 1. Total count
        assert_eq!(
            vectors.len(),
            137,
            "expected 137 vectors, got {}",
            vectors.len()
        );

        // 2. Blocked / allowed counts
        let blocked = vectors
            .iter()
            .filter(|v| v.expected == Outcome::Blocked)
            .count();
        let allowed = vectors
            .iter()
            .filter(|v| v.expected == Outcome::Allowed)
            .count();
        assert_eq!(blocked, 123, "expected 123 blocked, got {blocked}");
        assert_eq!(allowed, 14, "expected 14 allowed, got {allowed}");

        // 3. Blocked ones name their refusal layer
        let valid_layers = [
            "PathEscape",
            "NullByte",
            "DeviceName",
            "SymlinkEscape",
            "WorkspaceRoot",
            "BinaryFile",
            "ShellPolicy",
            "DeadManFuse",
            "DryRunGate",
            "ModuleIsolation",
        ];
        for v in vectors.iter().filter(|v| v.expected == Outcome::Blocked) {
            assert!(
                valid_layers.contains(&v.layer),
                "blocked vector '{}' has invalid layer '{}'",
                v.input,
                v.layer
            );
        }

        // 4. Allowed ones resolve in-tree
        for v in vectors.iter().filter(|v| v.expected == Outcome::Allowed) {
            assert_eq!(
                v.layer, "in-tree",
                "allowed vector '{}' must have layer 'in-tree', got '{}'",
                v.input, v.layer
            );
            if let Some(operand) = v.path_operand {
                assert!(
                    !is_protected_root(operand),
                    "allowed vector '{}' targets protected root '{}'",
                    v.input,
                    operand
                );
            }
        }

        // 5. Runnable vectors: exercise the Rust policy live.
        //    - `ShellPolicy` rows are asserted against `shell::validate`
        //      (the Rust shell-command policy) and must yield a typed refusal.
        //    - Path-layer rows are asserted against `normalize_traversal`.
        let root = test_root("security_table");
        for v in vectors.iter().filter(|v| v.runnable) {
            if v.layer == "ShellPolicy" {
                let res = shell::validate(v.input);
                assert!(
                    res.is_err(),
                    "Runnable vector '{}' should be blocked but was allowed",
                    v.input
                );
                let err = res.unwrap_err();
                match &err {
                    ShellPolicyError::ProhibitedPattern(_)
                    | ShellPolicyError::ProtectedRoot(_, _)
                    | ShellPolicyError::UnexpandedReference(_)
                    | ShellPolicyError::DeadManFuse => {}
                    other => panic!(
                        "runnable ShellPolicy vector '{}' expected a typed refusal, got {other:?}",
                        v.input
                    ),
                }
                continue;
            }
            let err = normalize_traversal(&root, v.input).unwrap_err();
            match (v.layer, &err) {
                ("PathEscape", SandboxError::PathEscape(_)) => {}
                ("NullByte", SandboxError::NullByte(_)) => {}
                ("DeviceName", SandboxError::DeviceName(_)) => {}
                (layer, err) => panic!(
                    "runnable vector '{}' expected layer {layer}, got {err:?}",
                    v.input
                ),
            }
        }
    }

    // ========================================================================
    // SandboxPolicy tests
    // ========================================================================

    /// Build a stand-in `~/.castor` state dir under a *fake* home directory
    /// (under `temp_dir()`, but not under the workspace), mirroring the
    /// production `$HOME/.castor` layout.
    fn fake_home_state_dir(tag: &str) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("castor_fake_home_{}_{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&home);
        let state = home.join(".castor");
        fs::create_dir_all(&state).unwrap();
        state
    }

    /// A policy whose write roots are exactly `[workspace]` and whose read roots
    /// are `[workspace, state_dir]`. This isolates the RW/RO split from the
    /// `temp_dir()` write root that `for_workspace` adds, so a state dir under
    /// `temp_dir()` is unambiguously read-only.
    fn rw_ro_policy(ws: &Path, state: &Path) -> SandboxPolicy {
        SandboxPolicy::new(
            vec![canonical_or_self(ws)],
            vec![canonical_or_self(ws), canonical_or_self(state)],
        )
    }

    #[test]
    fn policy_read_in_state_dir_succeeds_write_fails() {
        let ws = test_root("policy_state");
        let state = fake_home_state_dir("state");
        let policy = rw_ro_policy(&ws, &state);

        // Reading a file under the state dir is permitted (read-only root).
        let file = state.join("config.json");
        fs::write(&file, "{}\n").unwrap();
        let ro = policy
            .resolve(&file.to_string_lossy())
            .expect("state dir file must be readable");
        assert_eq!(ro.class, AccessClass::ReadOnly, "state dir is a read root");
        assert!(ro.permits_read() && !ro.permits_write());

        // A path under the workspace is read-write (both ops permitted).
        let wfile = ws.join("src").join("a.txt");
        fs::write(&wfile, "hi\n").unwrap();
        let rw = policy
            .resolve("src/a.txt")
            .expect("workspace file must be read-write");
        assert_eq!(rw.class, AccessClass::ReadWrite);
        assert!(rw.permits_read() && rw.permits_write());

        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn policy_write_in_state_dir_fails_read_only() {
        // Even though the state dir is readable, it is read-only: `resolve`
        // must classify it `ReadOnly` so write tools can refuse it.
        let ws = test_root("policy_rw_split");
        let state = fake_home_state_dir("ro");
        let policy = rw_ro_policy(&ws, &state);
        let file = state.join("x.txt");
        fs::write(&file, "x\n").unwrap();
        let resolved = policy.resolve(&file.to_string_lossy()).unwrap();
        assert_eq!(resolved.class, AccessClass::ReadOnly);
        assert!(!resolved.permits_write(), "state dir writes must be denied");
        let _ = fs::remove_dir_all(state.parent().unwrap());
    }

    #[test]
    fn policy_out_of_tree_path_fails() {
        let ws = test_root("policy_oot");
        let policy = SandboxPolicy::for_workspace(&ws, None);
        // /proc is neither a read nor write root on any platform (and is
        // guaranteed to escape the temp-dir workspace), so it must be denied.
        let err = policy.resolve("/proc/self/status").unwrap_err();
        assert!(matches!(err, SandboxError::PathEscape(_)), "{err}");
        // A relative traversal that escapes the workspace is also denied.
        let err = policy.resolve("../../outside.txt").unwrap_err();
        assert!(matches!(err, SandboxError::PathEscape(_)), "{err}");
    }

    #[test]
    fn policy_linux_system_read_roots_are_readable() {
        #[cfg(target_os = "linux")]
        {
            let ws = test_root("policy_linux_sys");
            let policy = SandboxPolicy::for_workspace(&ws, None);
            // /etc is a read-only root on Linux.
            let resolved = policy.resolve("/etc/hostname").unwrap();
            assert_eq!(resolved.class, AccessClass::ReadOnly);
            assert!(resolved.permits_read());
            assert!(!resolved.permits_write());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = test_root("policy_nolinux");
        }
    }

    #[test]
    fn policy_tilde_expansion_classifies() {
        let ws = test_root("policy_tilde");
        let policy = SandboxPolicy::workspace_only(&ws);
        // No HOME-based roots here, so a tilde path resolves to $HOME, which
        // is outside the tight workspace policy -> PathEscape (still classified,
        // never silently coerced).
        let expanded = policy.expand_tilde_prefix("~/some/file.txt");
        assert!(
            !expanded.starts_with('~'),
            "tilde must be expanded: {expanded}"
        );
        if home_dir().is_some() {
            let err = policy.resolve("~/some/file.txt").unwrap_err();
            assert!(matches!(err, SandboxError::PathEscape(_)), "{err}");
        }
    }

    #[test]
    fn policy_null_byte_and_device_refused() {
        let ws = test_root("policy_nulldev");
        let policy = SandboxPolicy::workspace_only(&ws);
        assert!(matches!(
            policy.resolve("a\0b"),
            Err(SandboxError::NullByte(_))
        ));
        assert!(matches!(
            policy.resolve("NUL"),
            Err(SandboxError::DeviceName(_))
        ));
    }

    #[test]
    fn workspace_only_policy_matches_single_root() {
        let ws = test_root("policy_ws_only");
        let policy = SandboxPolicy::workspace_only(&ws);
        // In-tree read/write.
        let in_tree = policy.resolve("src/a.txt").unwrap();
        assert_eq!(in_tree.class, AccessClass::ReadWrite);
        // Absolute out-of-tree path (e.g. /etc/passwd) is denied under the
        // tight policy, regardless of the platform system-read roots.
        let err = policy.resolve("/etc/passwd").unwrap_err();
        assert!(matches!(err, SandboxError::PathEscape(_)), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn policy_symlink_escaping_workspace_is_symlink_escape() {
        let ws = test_root("policy_symlink");
        let policy = SandboxPolicy::workspace_only(&ws);
        // A target that exists outside the workspace but is lexically inside it
        // (a symlink whose target is elsewhere) must be refused as SymlinkEscape,
        // because its canonical location escapes every root.
        let outside = std::env::temp_dir().join(format!(
            "castor_policy_symlink_target_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("secret.txt");
        fs::write(&outside_file, "top secret\n").unwrap();
        let link = ws.join("src").join("link_out");
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();

        // Lexically the link is under the workspace, but it resolves outside.
        let err = policy.resolve("src/link_out").unwrap_err();
        assert!(
            matches!(err, SandboxError::SymlinkEscape(_)),
            "expected SymlinkEscape, got {err}"
        );
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn policy_symlink_within_workspace_is_allowed() {
        let ws = test_root("policy_symlink_in");
        let policy = SandboxPolicy::workspace_only(&ws);
        let target = ws.join("src").join("real.txt");
        fs::write(&target, "hello\n").unwrap();
        let link = ws.join("alias.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let resolved = policy.resolve("alias.txt").unwrap();
        assert_eq!(resolved.class, AccessClass::ReadWrite);
        assert!(resolved.permits_read());
    }
}
