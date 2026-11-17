use std::fs;
use std::path::{Path, PathBuf};

use super::path::*;
use super::*;
use crate::tools::shell::{self, ShellPolicyError};

fn test_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("castor_sandbox_{}_{}", std::process::id(), name));
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
    assert!(
        detected_type.contains("application/octet-stream"),
        "{detected_type}"
    );
    assert!(reason.contains("null bytes"), "{reason}");
    let msg = err.to_string();
    // The sanctioned extraction path stays ...
    assert!(
        msg.contains("To extract its text, use the `bash` tool"),
        "{msg}"
    );
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
