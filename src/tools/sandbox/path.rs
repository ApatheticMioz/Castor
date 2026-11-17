//! Path containment, normalization, tilde expansion, and boundary checks.
//!
//! Provides layers 1–4 of the path containment policy:
//! 1. `resolve_workspace_root` — canonicalize the workspace root
//! 2. `verify_symlink_containment` — refuse symlink escapes
//! 3. `refuse_out_of_tree` — refuse absolute out-of-tree paths
//! 4. `normalize_traversal` — lexical normalization + traversal refusal

use std::fs;
use std::path::{Path, PathBuf};

use path_clean::PathClean;

use super::SandboxError;

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
pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
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
pub(crate) fn is_within_canonical(path: &Path, root: &Path) -> bool {
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
pub(crate) fn volume_is_case_insensitive(
    #[cfg_attr(windows, allow(unused_variables))] root: &Path,
) -> bool {
    #[cfg(windows)]
    {
        true
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
pub(crate) fn normalize_lexical(p: &Path) -> PathBuf {
    p.clean()
}

pub(crate) fn is_reserved_device(name: &str) -> bool {
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

/// Canonicalize `p` through the OS symlink/junction layer, falling back to
/// `p` unchanged when it cannot be resolved (a missing or vanished path).
/// Public so tests that build an explicit [`super::SandboxPolicy`] can canonicalize
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
pub(crate) fn canonical_resolve(p: &Path) -> PathBuf {
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

pub(crate) fn dedupe(v: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in v {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Canonical `$HOME` for the current user (platform-aware).
pub(crate) fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Expand a leading `~` / `~user` prefix to the canonical `$HOME` before
/// classification.
///
/// Per the spec, both a bare `~` and a `~user` form collapse to the
/// current user's canonical home (a conservative mapping that keeps every
/// tilde reference classifiable against the home-rooted read roots).
/// Trailing sub-paths are preserved: `~/a/b` → `$HOME/a/b`.
pub(crate) fn expand_tilde_prefix(input: &str) -> String {
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
    if let Some(sub) = after.strip_prefix('/').or_else(|| after.strip_prefix('\\')) {
        return join(sub); // "~/sub/..." or "~\sub\..."
    }
    // "~user" or "~user/sub": drop the `user` segment, keep the rest.
    let sub = after
        .split(['/', '\\'])
        .skip(1)
        .collect::<Vec<_>>()
        .join("/");
    join(&sub)
}
