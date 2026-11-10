//! 5-layer path containment policy.
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
    #[error("{0}{BINDING_DENIAL_DIRECTIVE}")]
    PathEscape(String),
    #[error("{0}{BINDING_DENIAL_DIRECTIVE}")]
    SymlinkEscape(String),
    #[error("{0}")]
    WorkspaceRoot(String),
    #[error(
        "BinaryFileError: '{file}' is a binary file ({detected_type}); reason: {reason}. \
         A text-only model must not ingest binary content. Do NOT read this file as text. \
         To extract its text, use the `bash` tool with a format-appropriate extractor \
         (e.g. 'pdftotext file -' for PDF, 'strings file', 'exiftool file', 'unzip -l file') \
         and read the extracted text output instead. \
         This is the sanctioned exception for extracting text from binary files only — \
         it does NOT authorize bypassing any other denial or the workspace's allowed roots."
    )]
    BinaryFile {
        file: PathBuf,
        detected_type: String,
        reason: String,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// The policy directive appended to every **binding** refusal message
/// (`PathEscape`, `SymlinkEscape`, and the read-only-root write denial built in
/// [`super::fs::FsExecutor::resolve_with`]).
///
/// Three jobs:
///   1. Frame the denial as a *policy decision* (not a transient failure), so
///      the model stops looking for a "tweak and retry."
///   2. Explicitly forbid the bypass the tool-routing prior otherwise invites
///      (`bash`/`cat`, alternative paths, other tools) — this is the
///      load-bearing line: without it a `read_file`/`write_file` denial reads as
///      a soft limit and `bash` becomes the default escape hatch (the shell
///      policy only blocks *destructive* commands, not path reads, so the
///      bypass would actually succeed).
///   3. Give the *correct* action: stop, and surface the constraint in the
///      final report to the Lead Architect / user, with the exact target and the
///      root/permission that would unblock it.
///
/// Kept as a single source of truth so every sandbox denial carries identical
/// guidance. The `bash`-specific `ShellPolicyError` variants in
/// [`super::shell`] carry an equivalent one-sentence directive inlined (a
/// proc-macro `#[error]` cannot reference this const); keep those in sync with
/// this text and with the `Security & Authority` invariant in
/// [`crate::mcp::worker::build_worker_system_prompt`].
pub const BINDING_DENIAL_DIRECTIVE: &str = " \
This refusal is a binding sandbox policy decision, not a transient failure. \
Do NOT attempt to bypass it with `bash` (or `cat`/`cp`/`mv`/`tee`), alternative paths, \
or any other tool. \
If this file or resource is essential to the task, stop and report the constraint \
in your final report to the Lead Architect / user, naming the exact target and the \
allowed root or permission that would unblock it.";

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
    dunce::canonicalize(&host_root).map_err(|e| {
        SandboxError::WorkspaceRoot(format!(
            "WorkspaceRootError: cannot canonicalize workspace root '{}': {e}",
            root.display()
        ))
    })
}

/// Whether `path` is **lexically** inside (or equal to) `root` — a pure
/// *spelling* comparison that never resolves symlinks.
///
/// The primary check is an exact, **byte-wise** prefix match. Rust's `Path`
/// comparison is byte-wise on *every* OS — **including Windows** — so a
/// differently-cased spelling of the *same* location does not match by bytes
/// on a case-insensitive store (Windows NTFS, or WSL's `/mnt/[a-z]` drvfs
/// drive mounts, where `D:\` ≡ `d:\` and `/mnt/c/Users/Apath` ≡
/// `/mnt/c/Users/apath`). A component-boundary-aware case-insensitive prefix
/// check is therefore the fallback that recognizes such a spelling as the same
/// location.
///
/// On Windows the fallback always applies (NTFS is case-insensitive by
/// default). On Linux it is gated by a **canonical-identity** volume probe
/// ([`volume_is_case_insensitive`] on the root's store) rather than a
/// `/mnt/[a-z]` string heuristic: the fallback runs only when the actual
/// filesystem reports the store case-insensitive, so a bare (case-sensitive)
/// ext4/btrfs store is never made over-permissive, and a `/mnt/<x>` path that
/// is merely a local directory is correctly rejected.
///
/// This is deliberately *lexical only*: the containment policy uses it to tell
/// a **symlink escape** (the *spelling* is in-tree but the *real* location is
/// not) apart from a plain out-of-tree path. The symlink-dereferenced variant
/// that canonicalizes both ends first is [`is_within_canonical`].
fn is_within(path: &Path, root: &Path) -> bool {
    if path.strip_prefix(root).is_ok() {
        return true;
    }
    #[cfg(any(windows, target_os = "linux"))]
    {
        let p_ci = case_insensitive_form(path);
        let r_ci = case_insensitive_form(root);
        // Windows: the CI fallback always applies. Linux: gate on the
        // canonical-identity volume probe of the root's store (a path lexically
        // under the root lives on the same mount) so a case-sensitive store is
        // never over-permissive.
        let ci = if cfg!(windows) {
            true
        } else {
            volume_is_case_insensitive(&canonical_resolve(root))
        };
        if ci {
            let r_trim = r_ci.trim_end_matches('/');
            if p_ci == r_ci || p_ci.starts_with(&format!("{r_trim}/")) {
                return true;
            }
        }
    }
    false
}

/// Whether `path` lands inside `root` **by canonical identity**: both ends are
/// resolved to their true (symlink/junction-dereferenced) location via
/// [`canonical_resolve`] before the comparison, so a symlinked allowed root
/// and any path beneath it compare like-for-like against the root's canonical
/// form.
///
/// The primary check is an exact **byte-wise** prefix match on the canonical
/// forms. On a case-insensitive store a differently-cased spelling of the
/// *same* real location does not match by bytes, so a case-insensitive prefix
/// fallback applies when the root's volume is one — decided by the
/// *canonical-identity* probe [`volume_is_case_insensitive`] (not a string
/// prefix): a spelling of the root with its deepest alphabetic component
/// re-cased resolves to the same location iff the volume is case-insensitive.
/// On Windows the volume is always treated as case-insensitive (NTFS); on
/// Linux a bare ext4/btrfs volume reports "sensitive", so a mixed-case
/// *sibling* is still denied there.
fn is_within_canonical(path: &Path, root: &Path) -> bool {
    let real_path = canonical_resolve(path);
    let real_root = canonical_resolve(root);
    if real_path.strip_prefix(&real_root).is_ok() {
        return true;
    }
    #[cfg(any(windows, target_os = "linux"))]
    {
        // Windows: NTFS is case-insensitive by default, so the fallback always
        // applies. Linux: gate on the canonical-identity volume probe.
        let ci = if cfg!(windows) {
            true
        } else {
            volume_is_case_insensitive(&real_root)
        };
        if ci {
            let p_ci = case_insensitive_form(&real_path);
            let r_ci = case_insensitive_form(&real_root);
            let r_trim = r_ci.trim_end_matches('/');
            if p_ci == r_ci || p_ci.starts_with(&format!("{r_trim}/")) {
                return true;
            }
        }
    }
    false
}

/// Forward-slash, lowercased form of a path (Windows separators normalized to
/// `/` first).
#[cfg(any(windows, target_os = "linux"))]
fn case_insensitive_form(p: &Path) -> String {
    let s = p.to_string_lossy();
    #[cfg(windows)]
    {
        s.replace('\\', "/").to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        s.to_ascii_lowercase()
    }
}

/// Whether the store containing the canonical path `root` is case-insensitive
/// — the predicate that decides whether a differently-cased spelling of a
/// location may match its canonical form.
///
/// This is a *canonical-identity* test, not a string-prefix heuristic: the
/// root's final path component is re-cased (its ASCII letters flipped) to form
/// an alternative spelling of the *same* location, and both spellings are
/// canonicalized through the OS symlink/junction layer. They resolve to the
/// same real location **if and only if** the store is case-insensitive. On a
/// case-sensitive store the re-cased spelling resolves to a distinct (or
/// nonexistent) location, so the test is false there even if the path shares a
/// drive-mount shape with one (e.g. a Linux `/mnt/d` that is a local directory,
/// not a Windows drive).
///
/// Windows always reports `true` (NTFS / drvfs are case-insensitive in the
/// default configuration). On Linux the probe is a no-op (and returns `false`)
/// when the root's final component has no ASCII letters, so there is no
/// re-cased spelling to test — a conservative default that only ever narrows,
/// never widens, the set of matched spellings.
#[cfg(any(windows, target_os = "linux"))]
fn volume_is_case_insensitive(root: &Path) -> bool {
    #[cfg(windows)]
    {
        return true;
    }
    #[cfg(not(windows))]
    {
        let Some(name) = root.file_name() else {
            return false;
        };
        let s = name.to_string_lossy();
        // A component without ASCII letters has no differently-cased spelling,
        // so there is nothing to probe (and no risk of over-permission).
        if !s.chars().any(|c| c.is_ascii_alphabetic()) {
            return false;
        }
        let re_cased = if s.starts_with(|c: char| c.is_ascii_uppercase()) {
            s.to_ascii_lowercase()
        } else {
            s.to_ascii_uppercase()
        };
        if re_cased == s {
            return false;
        }
        let probe = root
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .join(&re_cased);
        // A canonicalized re-cased spelling equal to the canonical root means
        // the filesystem mapped the two spellings to the same location.
        canonical_or_self(&probe) == canonical_or_self(root)
    }
}

/// Layer 2: refuse symlink escapes. An existing target must canonicalize
/// inside the root; a non-existent target's nearest existing ancestor must
/// canonicalize inside the root.
pub fn verify_symlink_containment(target: &Path, root: &Path) -> Result<PathBuf, SandboxError> {
    if target.exists() {
        let real = dunce::canonicalize(target)?;
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
            let real = dunce::canonicalize(&p)?;
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
    let s = target.to_string_lossy();
    let is_root_anchored = target.is_absolute()
        || s.starts_with('/')
        || s.starts_with('\\')
        || (s.len() >= 2 && s.as_bytes()[1] == b':' && s.as_bytes()[0].is_ascii_alphabetic());

    if is_root_anchored && !is_within(target, root) {
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

/// Layer 5: binary read fail-fast. Inspect the first 4096 bytes with
/// `content_inspector::inspect`: binary content (null bytes / non-text BOM)
/// yields `SandboxError::BinaryFile`, carrying the descriptive mime/extension
/// from `infer::get` when a known signature matches, or `application/octet-stream`
/// otherwise. Text content passes through.
pub fn check_binary(path: &Path) -> Result<PathBuf, SandboxError> {
    let mut f = fs::File::open(path)?;
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    if content_inspector::inspect(&buf).is_binary() {
        let (detected_type, reason) = match infer::get(&buf) {
            Some(kind) => (
                format!("{} ({})", kind.mime_type(), kind.extension()),
                format!("magic bytes identify it as {}", kind.mime_type()),
            ),
            None => (
                "application/octet-stream (binary)".to_string(),
                "null bytes detected in file header".to_string(),
            ),
        };
        return Err(SandboxError::BinaryFile {
            file: path.to_path_buf(),
            detected_type,
            reason,
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

/// Canonicalize `p` through the OS symlink/junction layer, falling back to
/// `p` unchanged when it cannot be resolved (a missing or vanished path).
/// Public so tests that build an explicit [`SandboxPolicy`] can canonicalize
/// their roots the same way the policy itself does.
pub fn canonical_or_self(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Resolve `p` to its real (symlink/junction-dereferenced) location, even when
/// `p` itself does not yet exist.
///
/// When `p` exists, the whole chain is canonicalized. When `p` does not exist
/// but some ancestor does, the nearest existing ancestor is canonicalized and
/// the missing tail is re-appended. This is what makes a *symlinked allowed
/// root* (e.g. WSL's `/home/apath/.castor -> /mnt/c/Users/apath/.castor`) and
/// any file beneath it — including a spill file the runner just created or a
/// new file the model is about to write — resolve to the same canonical
/// location the allowed roots use, so containment checks compare like-for-like
/// instead of mismatching a symlink spelling against a canonical root.
fn canonical_resolve(p: &Path) -> PathBuf {
    if p.exists() {
        return canonical_or_self(p);
    }
    // Walk up to the nearest existing ancestor, collecting the missing tail.
    // Each step strictly shortens the path, so the loop terminates at the
    // filesystem root (or the first existing ancestor).
    let mut tail: Vec<&Path> = Vec::new();
    let mut cur = p;
    loop {
        match cur.parent() {
            Some(parent) if !parent.as_os_str().is_empty() && parent != cur => {
                if let Some(name) = cur.file_name() {
                    tail.push(Path::new(name));
                }
                cur = parent;
                if cur.exists() {
                    break;
                }
            }
            _ => break,
        }
    }
    let mut out = canonical_or_self(cur);
    for seg in tail.iter().rev() {
        out = out.join(seg);
    }
    out
}

fn dedupe(v: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in v {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
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
/// normalization to `normalize_lexical` and device-name refusal to
/// `is_reserved_device` so those invariants are preserved.
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
    /// eval runner, where only the workspace itself may be touched.
    pub fn workspace_only(root: &Path) -> Self {
        let ws = canonical_or_self(root);
        Self::new(vec![ws.clone()], vec![ws])
    }

    /// The production policy built by [`crate::tools::CompositeExecutor`]:
    ///
    /// - **write roots**: canonicalized `workspace_root` + `std::env::temp_dir()`
    /// - **read roots**: canonicalized `workspace_root` + optional `state_dir`
    ///   (`~/.castor`) + (on Linux) `/usr`, `/bin`, `/lib`, `/etc`
    ///
    /// Both the workspace root and the state dir are resolved to their *true*
    /// canonical paths **once, here, at construction** (via the OS
    /// symlink/junction layer), and the `.scratch/` spill subdirectory is
    /// canonicalized too. Every later membership check therefore compares
    /// canonical-to-canonical: a `~/.castor` that is a symlink (e.g. WSL's
    /// `/home/apath/.castor -> /mnt/c/Users/apath/.castor`) and the spill files
    /// the runner writes under it both resolve to the same canonical location,
    /// so reads through either spelling succeed unconditionally.
    pub fn for_workspace(root: &Path, state_dir: Option<&Path>) -> Self {
        // Single canonical resolution of the workspace root (and, if supplied,
        // the state dir) so every root this policy holds is a true canonical
        // path, never a symlink or differently-cased spelling.
        let ws = canonical_or_self(root);
        let write_roots = vec![ws.clone(), canonical_or_self(&std::env::temp_dir())];
        let mut read_roots = vec![ws];
        if let Some(sd) = state_dir {
            let sd_c = canonical_or_self(sd);
            // The `.scratch/` spill directory is always a valid read target:
            // canonicalize it (when it exists) so a symlinked state dir never
            // splits the spill path from its read root.
            let scratch = sd_c.join(".scratch");
            if scratch.is_dir() {
                read_roots.push(canonical_or_self(&scratch));
            }
            read_roots.push(sd_c);
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

    /// Classify a raw path string against the policy, resolving any *relative*
    /// path against the primary write root (the workspace).
    ///
    /// Order of operations (preserving the 5-layer invariants, generalized to the
    /// root sets):
    /// 1. null-byte refusal (layer 4),
    /// 2. tilde expansion to canonical `$HOME`,
    /// 3. backslash→slash normalization + lexical `.`/`..` collapse (layer 4),
    /// 4. reserved-device-name refusal (layer 4),
    /// 5. canonical membership: the path's *real* (symlink-dereferenced)
    ///    location — resolved through the nearest existing ancestor when the
    ///    path does not yet exist — must land inside a root. If it does not:
    ///    a [`SandboxError::SymlinkEscape`] when the lexical spelling was
    ///    in-tree (the link points outside), else a plain out-of-tree
    ///    [`SandboxError::PathEscape`]. This ordering means a symlinked
    ///    allowed root (and anything beneath it) always resolves consistently,
    ///    while a symlink that escapes the roots is still refused;
    /// 6. the granted class reflects the *canonical* location — `ReadWrite` if
    ///    under a write root, else `ReadOnly` if under a read root — so a symlink
    ///    from a write root into a read-only root is treated as read-only, never
    ///    read-write.
    ///
    /// The returned [`ResolvedPath::path`] is the lexically-normalized path (stable
    /// and printable); its `class` is derived from the canonical location.
    pub fn resolve(&self, input: &str) -> Result<ResolvedPath, SandboxError> {
        self.resolve_with_base(input, None)
    }

    /// Classify `input` against the policy, resolving any *relative* path
    /// against `base` instead of the primary write root (the workspace).
    ///
    /// The filesystem tools use this to honor the active task's working
    /// directory (`cwd`): a relative `path` is resolved against the cwd (which
    /// may be a subdirectory of the workspace, or the workspace root itself
    /// when no cwd is set) before containment is checked against the roots.
    ///
    /// **Single-root canonicalization.** Both the resolution base and the
    /// target are canonicalized through the OS symlink/junction layer before
    /// any containment decision: the base (relative-path anchor) and the
    /// target (the path being classified) are each reduced to their *true*
    /// canonical location so that a symlinked cwd — or a symlinked allowed
    /// root — never mismatches its canonical spelling (see
    /// [`canonical_resolve`]). The returned [`ResolvedPath::path`] is still the
    /// *lexically* normalized path (stable and printable); only the *decision*
    /// uses the canonical location. Callers that need `base` itself to be an
    /// allowed location must validate that separately (it is the caller's
    /// anchor, not a path the model asked for).
    pub fn resolve_with_base(
        &self,
        input: &str,
        base: Option<&Path>,
    ) -> Result<ResolvedPath, SandboxError> {
        if input.contains('\0') {
            return Err(SandboxError::NullByte(
                "NullByteError: Path contains prohibited null byte character".into(),
            ));
        }
        let expanded = self.expand_tilde_prefix(input);
        let posix = expanded.replace('\\', "/");
        // A relative path is resolved against the explicit base (the active
        // task cwd) when one is supplied, otherwise the primary write root
        // (the workspace). An absolute path is used as-is.
        let base = base
            .map(normalize_lexical)
            .unwrap_or_else(|| self.write_roots.first().cloned().unwrap_or_default());
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

        // (5) Canonical membership: resolve the target to its real
        // (symlink/junction-dereferenced) location — through the nearest
        // existing ancestor when the path does not yet exist — so a symlinked
        // allowed root (e.g. `/home/apath/.castor -> /mnt/c/Users/apath/.castor`)
        // and any file beneath it compare against the canonical roots by their
        // *real* location. `is_within_canonical` canonicalizes each side (path
        // and root) through the OS symlink/junction layer before comparing, so
        // the real location and the roots always compare like-for-like.
        let real = canonical_resolve(&normalized);
        let real_in_write = self
            .write_roots
            .iter()
            .any(|r| is_within_canonical(&real, r));
        let real_in_read = self.read_roots.iter().any(|r| is_within_canonical(&real, r));
        // Lexical membership: does the (symlink-free) spelling sit inside a
        // root? Used only to distinguish a symlink escape (lexical in, real
        // out) from a plain out-of-tree path (both out). This stays a pure
        // spelling check (never canonicalizes) so the two senses remain
        // distinct.
        let lexical_in_write = self.write_roots.iter().any(|r| is_within(&normalized, r));
        let lexical_in_read = self.read_roots.iter().any(|r| is_within(&normalized, r));
        if !real_in_write && !real_in_read {
            // The lexical spelling is in-tree but the *real* location is not:
            // a symlink escape (the target points outside every allowed root).
            if lexical_in_write || lexical_in_read {
                return Err(SandboxError::SymlinkEscape(format!(
                    "SymlinkEscapeError: Path '{}' resolves to '{}' escaping all allowed roots",
                    normalized.display(),
                    real.display()
                )));
            }
            // Lexically out of every root (and its real location is too): a
            // plain out-of-tree path, not a symlink error.
            return Err(SandboxError::PathEscape(format!(
                "PathEscapeError: Access denied. Path '{}' escapes all allowed roots",
                input
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
        assert_eq!(resolved, dunce::canonicalize(&root).unwrap());
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
    // Case-sensitivity of `is_within`
    //
    // Rust's `Path` comparison is byte-wise on every OS, so a mixed-case
    // spelling of a location on a case-insensitive store (Windows NTFS, or a
    // drvfs `/mnt/<x>` drive mount under WSL) does NOT match by bytes. The
    // fallback that recognizes such a spelling as the same location is gated on
    // a *canonical-identity* volume probe, not on a path-prefix heuristic.
    // These tests run against real directories so the probe is exercised
    // against the actual filesystem of whatever host runs the suite:
    //   (a) on a case-insensitive volume (NTFS / a drvfs mount) a
    //       mixed-case spelling of an existing root matches it;
    //   (b) on a bare case-sensitive volume (the common Linux /tmp on ext4)
    //       the same mixed-case spelling is DENIED, and the volume probe
    //       itself reports the volume as case-sensitive.
    // ========================================================================

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn canonical_identity_case_insensitivity() {
        let root = test_root("ci_identity");
        // A differently-cased spelling of the *root itself* matches iff the
        // volume is case-insensitive — the primary byte-wise check fails on
        // the case difference, so a `true` outcome proves the canonical
        // fallback, and a `false` outcome proves a case-sensitive store is
        // never over-permissive.
        let re_cased = canonical_or_self(&root);
        let mixed = re_cased
            .file_name()
            .map(|f| {
                let s = f.to_string_lossy().into_owned();
                let flipped = if s.starts_with(|c: char| c.is_ascii_uppercase()) {
                    s.to_ascii_lowercase()
                } else {
                    s.to_ascii_uppercase()
                };
                re_cased.parent().unwrap_or(Path::new("/")).join(flipped)
            })
            .expect("test root has a file name");
        let matches = is_within(&mixed, &root);
        assert_eq!(
            matches,
            volume_is_case_insensitive(&canonical_or_self(&root)),
            "the is_within fallback must agree with the canonical-identity \
             volume probe on the host's real filesystem"
        );
        // A *different* location with the same lowercased form is never
        // treated as inside (no byte-wise or case-insensitive prefix match
        // can relate them), regardless of the store's case policy.
        let other = test_root("ci_identity_other");
        assert!(!is_within(&other, &root));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_drive_linux_path_is_case_sensitive() {
        // On a bare (case-sensitive) Linux filesystem, the case-insensitive
        // fallback must NOT apply: a mixed-case path is denied against a
        // differently-cased root, even though their lowercased forms are
        // equal. The canonical-identity volume probe is what prevents the
        // fallback from leaking over-permission onto ext4/btrfs.
        let root = Path::new("/tmp/sandbox_a");
        let p = Path::new("/tmp/Sandbox_A/file.txt");
        assert!(
            !is_within(p, root),
            "a mixed-case non-drive Linux path must be DENIED against a \
             differently-cased root (case-sensitive store)"
        );
        // The exact, same-case spelling still matches — we deny only the
        // *different-case* spelling, not the identical location.
        assert!(
            is_within(Path::new("/tmp/sandbox_a/file.txt"), root),
            "the identical-case path must still match"
        );
    }

    // ========================================================================
    // 137-Vector Security Table
    // (123 attack vectors blocked + 14 allow vectors permitted)
    //
    // Each row: (input, op, expected, layer, path_operand, runnable)
    //   - `layer` names the refusal layer for blocked vectors, or "in-tree"
    //     for allowed ones.
    //   - `runnable` = true when the vector can be exercised live against a
    //     Rust policy: `ShellPolicy` rows through `shell::validate`, path
    //     rows through `normalize_traversal`. false for policy rules
    //     (deadman-fuse / dry-run / module-isolation) mapped to the closest layer.
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
        // Writing a file over the root directory. Closest Rust
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
        // Synthetic canary token mapped to DeadManFuse.
        v.push(Vector {
            input: "CANARY_DISASTER_FUSE_TOKEN",
            op: None,
            expected: Outcome::Blocked,
            layer: "DeadManFuse",
            path_operand: None,
            runnable: false,
        });

        // --- Category 5d: Dry-Run Hard Gate (1) ---
        // Dry-run simulation mapped to DryRunGate.
        v.push(Vector {
            input: "echo safe_dry_run_simulation",
            op: None,
            expected: Outcome::Blocked,
            layer: "DryRunGate",
            path_operand: None,
            runnable: false,
        });

        // --- Category 5e: Structural Module Isolation (1) ---
        // Zero-child-process guarantee mapped to ModuleIsolation.
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
                assert!(
                    matches!(
                        res,
                        Err(ShellPolicyError::ProhibitedPattern(_)
                            | ShellPolicyError::ProtectedRoot(_, _)
                            | ShellPolicyError::UnexpandedReference(_)
                            | ShellPolicyError::DeadManFuse)
                    ),
                    "runnable ShellPolicy vector '{}' expected a typed refusal, got {res:?}",
                    v.input
                );
                continue;
            }
            let err = normalize_traversal(&root, v.input).unwrap_err();
            let ok = matches!(
                (v.layer, &err),
                ("PathEscape", SandboxError::PathEscape(_))
                    | ("NullByte", SandboxError::NullByte(_))
                    | ("DeviceName", SandboxError::DeviceName(_))
            );
            assert!(
                ok,
                "runnable vector '{}' expected layer {}, got {err:?}",
                v.input, v.layer
            );
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

    // ========================================================================
    // Denial-directive tests
    //
    // The binding security refusals (`PathEscape`, `SymlinkEscape`) must
    // carry the anti-bypass + escalation directive verbatim, so the coworker
    // reports the constraint to the Lead Architect / user instead of routing
    // around the denial with `bash`. These lock that text in place.
    // ========================================================================

    #[test]
    fn path_escape_denial_carries_binding_directive() {
        let ws = test_root("directive_escape");
        let policy = SandboxPolicy::workspace_only(&ws);
        // Absolute out-of-tree path under the tight policy.
        let err = policy.resolve("/etc/passwd").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("PathEscapeError"),
            "denial must keep its type tag: {msg}"
        );
        for needle in [
            "binding sandbox policy decision",
            "Do NOT attempt to bypass",
            "`bash`",
            "report the constraint",
            "Lead Architect / user",
            "allowed root or permission",
        ] {
            assert!(msg.contains(needle), "denial missing '{needle}': {msg}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_denial_carries_binding_directive() {
        let ws = test_root("directive_symlink");
        let policy = SandboxPolicy::workspace_only(&ws);
        let outside = std::env::temp_dir().join(format!(
            "castor_directive_symlink_target_{}_{}",
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

        let err = policy.resolve("src/link_out").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SymlinkEscapeError"),
            "denial must keep its type tag: {msg}"
        );
        for needle in [
            "binding sandbox policy decision",
            "Do NOT attempt to bypass",
            "`bash`",
            "report the constraint",
            "Lead Architect / user",
        ] {
            assert!(msg.contains(needle), "denial missing '{needle}': {msg}");
        }
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn binary_file_denial_scopes_the_bash_exception() {
        let ws = test_root("directive_binary");
        let bin = ws.join("blob.bin");
        fs::write(&bin, [0x00, 0x01, 0x02, 0xFF, 0xFE]).unwrap();
        let err = check_binary(&bin).unwrap_err();
        let SandboxError::BinaryFile {
            detected_type,
            reason,
            ..
        } = &err
        else {
            panic!("expected BinaryFile, got {err:?}");
        };
        assert!(detected_type.contains("application/octet-stream"), "{detected_type}");
        assert!(reason.contains("null bytes"), "{reason}");
        let msg = err.to_string();
        // The sanctioned extraction path stays ...
        assert!(msg.contains("To extract its text, use the `bash` tool"), "{msg}");
        // ... but is explicitly scoped so it is never read as a general bypass.
        assert!(
            msg.contains("sanctioned exception for extracting text from binary files only"),
            "binary denial must scope the bash exception: {msg}"
        );
        assert!(
            msg.contains("does NOT authorize bypassing any other denial"),
            "binary denial must not authorize bypassing other denials: {msg}"
        );
    }
}
