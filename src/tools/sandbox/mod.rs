//! 5-layer path containment policy and symmetric sandbox policy.
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

pub mod binary;
pub mod path;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use thiserror::Error;

pub use binary::check_binary;
pub use path::{
    canonical_or_self, normalize_traversal, refuse_out_of_tree, resolve_workspace_root,
    verify_symlink_containment,
};

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

/// Symmetric filesystem policy: a set of read-write roots and a (superset) set
/// of read-only roots.
///
/// This is the single source of truth that both the filesystem tools and the
/// (Linux) Landlock confinement derive their allowed-path classification from,
/// so the two layers never disagree.
///
/// Layers 1–5 (workspace-root resolution, symlink containment, out-of-tree
/// refusal, lexical normalization, binary sniff) remain intact as free
/// functions in submodules; this policy *generalizes* the single-root model to a
/// read/write split and adds tilde expansion, while delegating lexical
/// normalization to `path::normalize_lexical` and device-name refusal to
/// `path::is_reserved_device` so those invariants are preserved.
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
            write_roots: path::dedupe(write_roots),
            read_roots: path::dedupe(read_roots),
        }
    }

    /// The tight policy used by [`crate::tools::fs::FsExecutor::new`] and the
    /// eval runner, where only the workspace itself may be touched.
    pub fn workspace_only(root: &Path) -> Self {
        let ws = path::canonical_or_self(root);
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
        let ws = path::canonical_or_self(root);
        let write_roots = vec![ws.clone(), path::canonical_or_self(&std::env::temp_dir())];
        let mut read_roots = vec![ws];
        if let Some(sd) = state_dir {
            let sd_c = path::canonical_or_self(sd);
            // The `.scratch/` spill directory is always a valid read target:
            // canonicalize it (when it exists) so a symlinked state dir never
            // splits the spill path from its read root.
            let scratch = sd_c.join(".scratch");
            if scratch.is_dir() {
                read_roots.push(path::canonical_or_self(&scratch));
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
    pub fn expand_tilde_prefix(&self, input: &str) -> String {
        path::expand_tilde_prefix(input)
    }

    /// Classify a raw path string against the policy, resolving any *relative*
    /// path against the primary write root (the workspace).
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
    /// [`path::canonical_resolve`]). The returned [`ResolvedPath::path`] is still the
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
            .map(path::normalize_lexical)
            .unwrap_or_else(|| self.write_roots.first().cloned().unwrap_or_default());
        let p = if Path::new(&posix).is_absolute() {
            PathBuf::from(posix)
        } else {
            base.join(&posix)
        };
        let normalized = path::normalize_lexical(&p);
        if let Some(name) = normalized.file_name()
            && path::is_reserved_device(&name.to_string_lossy())
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
        let real = path::canonical_resolve(&normalized);
        let real_in_write = self
            .write_roots
            .iter()
            .any(|r| path::is_within_canonical(&real, r));
        let real_in_read = self
            .read_roots
            .iter()
            .any(|r| path::is_within_canonical(&real, r));
        // Lexical membership: does the (symlink-free) spelling sit inside a
        // root? Used only to distinguish a symlink escape (lexical in, real
        // out) from a plain out-of-tree path (both out). This stays a pure
        // spelling check (never canonicalizes) so the two senses remain
        // distinct.
        let lexical_in_write = self
            .write_roots
            .iter()
            .any(|r| path::is_within(&normalized, r));
        let lexical_in_read = self
            .read_roots
            .iter()
            .any(|r| path::is_within(&normalized, r));
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
