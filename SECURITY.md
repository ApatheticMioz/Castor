# Security Policy

**Castor** — The Universal 245K Agent Microkernel for Local LLMs.

This document describes how to report a security vulnerability in Castor
responsibly, what we consider in-scope, and how we handle reports.

## Supported Versions

| Version | Supported |
| ------- | --------- |
| Latest `main` / latest release | Yes |
| Previous release | Case-by-case (we will advise) |
| Anything older | No |

Please report vulnerabilities against the **latest release** or `main`. If you
believe an older version is affected, note it in your report and we will
assess backporting.

## Reporting a Vulnerability

**Please do NOT open a public GitHub issue for a security vulnerability.**
Public issues are visible to everyone, including the people who would exploit
the flaw before a fix ships.

### Preferred: Private disclosure

1. **Email the maintainer** at [`ApatheticMioz@gmail.com`](mailto:ApatheticMioz@gmail.com),
   with the subject line **`[Security] <short description>`**.
2. Include, as far as you can safely provide:
   - A clear description of the vulnerability and its impact.
   - The affected component (e.g. zero-trust sandbox, shell hardening /
     command validator, path canonicalization, MCP server, stream proxy,
     evo lineage, WSL bridge).
   - **Reproducible steps** or a minimal proof-of-concept.
   - The version / commit you tested against.
   - Any mitigation you discovered.
3. If you have a way to encrypt your report (e.g. PGP), please use it.
   We do not currently publish a PGP key; use the private channel in
   `CONTRIBUTING.md` or GitHub's private security advisory feature.

### What to include (and what to redact)

- **Include**: repro steps, error output, affected code paths, version.
- **Redact**: API keys, tokens, private keys, personal home-directory
  paths, and any other credentials. Castor is a *local-first* tool — your
  machine paths and keys are yours, not ours.

## What We Consider a Security Vulnerability

Castor is a **local-first, zero-trust** agent harness. The following are
in-scope and treated as security issues:

- **Sandbox escape** — a tool call, patch, or shell command writing or
  reading **outside** the project root that the zero-trust sandbox is
  supposed to contain (path traversal, symlink/junction escape, UNC path
  tricks, `..` sequences, null-byte injection).
- **Shell / command injection** — a user-controlled value breaking out of
  the shell-quoting / ERE-escaping in the shell executor or the anchored
  WSL process sweep (e.g. a session id that over-kills sibling processes).
- **Command-validator bypass** — a destructive or privileged command
  (e.g. `rm -rf /`, `powershell -Command …`, `cmd /c …`) that the
  hardening validator fails to block.
- **Credential / secret exposure** — a code path that writes an API key,
  token, or private key to a log, a committed file, or a world-readable
  location.
- **MCP server / stream-proxy attack surface** — unauthenticated or
  mis-routed network endpoints, request-smuggling, or stdio-purity
  violations that let a client inject control traffic.
- **Supply-chain** — a dependency that introduces a known malicious or
  vulnerable package.

## What Is Out of Scope

- Behavior that is **documented** and intended (e.g. the microkernel
  executing the commands you, the operator, explicitly ask it to run).
- Issues that require **physical access** to your machine or a compromised
  / root account.
- Theoretical issues with no realistic attack path in a local-first
  deployment.
- Vulnerabilities in **upstream** projects (Rust, Cargo, vLLM, the LLM
  weights, `ast-grep`, `rmcp`) — please report those to the
  respective maintainers; we will coordinate if it affects Castor.

## Our Commitments

- **Acknowledgement** within **3 business days** of receiving a valid
  report.
- **Triage** and a severity assessment within **7 calendar days**.
- **Fix** on a best-effort basis, prioritized by severity and exploitability.
  Critical / actively-exploitable issues are fast-tracked.
- **Credit** in the release notes, **if you want it** (we will never
  publish your name or contact details without explicit permission).
- **No public disclosure** of the vulnerability or the reporter's identity
  before a fix is available, unless required by law.

## Severity Guide

| Severity | Definition | Target Response |
| -------- | ---------- | --------------- |
| **Critical** | Sandbox escape, RCE, or credential theft reachable from a normal tool call. | Immediate / same-day |
| **High** | Validator bypass, over-kill of sibling processes, or a contained-but-real escape. | ≤ 7 days |
| **Medium** | Information disclosure, denial-of-service of the local engine, or a hardening gap with a narrow attack path. | ≤ 30 days |
| **Low** | Hardening improvements, defense-in-depth gaps, or issues requiring unusual preconditions. | Best-effort |

## If You Find a Vulnerability in a Dependency

Report it to the dependency's maintainer first, and let us know so we can
pin / patch on our side. We track dependencies via `Cargo.lock` and
`cargo audit` in CI.

---

**Thank you for helping keep Castor safe.** Responsible disclosure makes the
local-first, zero-trust model actually hold up in the wild.
