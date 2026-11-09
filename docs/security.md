# Workspace Containment

<img src="assets/security_sandbox.svg"
     alt="Castor workspace containment" width="100%">

Castor enforces zero-trust containment at every tool boundary:

| Stage | Mechanism | What it blocks |
|---|---|---|
| **1. Path normalizer** | Canonical path resolution & device namespace neutralization | `../../etc/passwd`, `C:\`, `NUL`, `CON` |
| **2. Realpath anchor** | Symlink traversal audit & workspace boundary guard (`refuse_out_of_tree`) | Symlink escapes, out-of-workspace writes, system directories |
| **3. Command filter** | Lexical shell command scanner & signature blocks (`shell.rs`) | `rm -rf /`, `format`, `dd`, fork bombs, `mkfs`, shell injection bypasses |
| **4. AST syntax gate** | Structural syntax parser validation for `edit_file` and `ast_replace` before disk commit (`[feature: ast]`) | In-memory rollback on syntax error across Rust, TS, JS, Python, JSON |

File edits in `fs.rs` use atomic fsync writes via `NamedTempFile` to prevent torn states.
Structural mutations via `ast_replace` validate syntax in-memory before writing to disk,
atomically rolling back if parsing fails.

The canonical specification of the containment pipeline is [`AGENTS.md`](../AGENTS.md) §6.
To report a vulnerability, follow [`SECURITY.md`](../SECURITY.md); never open a public issue.
