# Castor: Frequently Asked Questions (FAQ)

> Comprehensive technical, commercial, and regulatory FAQ for Castor Technologies.

---

## 1. Technical & Architecture Questions

### Q1: Why did you build Castor in Rust instead of Python?
**A**: Most AI agent frameworks (e.g. LangChain, CrewAI, AutoGen, OpenHands) are written in Python or TypeScript. While easy to prototype, they suffer from high memory overhead, unpredictable garbage collection pauses, fragile subprocess management, and poor sandbox security.  
Castor is written in Rust (2024 edition) as an in-process agent microkernel. It dispatches tool calls in sub-millisecond time (<0.1ms), enforces strict memory safety via `mimalloc`, and embeds tree-sitter AST parsers directly into memory without spawning external child processes for every edit.

### Q2: What is the "HyperQwen MTP" serving recipe?
**A**: HyperQwen is our specialized serving configuration for `Qwen3.8-27B-W4A16-AutoRound`. It combines:
1. **AutoRound W4A16 Quantization**: Compresses the 27B model into ~17 GB of VRAM while preserving >99% perplexity of full FP16 weights.
2. **Multi-Token Prediction (MTP k=3)**: Speculative decoding where the model predicts 3 future tokens simultaneously in a single forward pass, verified by the base model. This boosts sustained decode speed to 280 tokens/sec across 4 concurrent streams with peak speculative bursts hitting 1,995 tok/s.
3. **PagedAttention & Prefix Caching**: Pre-caches the Turn-1 system prompt and workspace skeleton, achieving a 93.7% KV-cache hit rate.

### Q3: How does the In-Memory AST Syntax Gate work?
**A**: When an autonomous agent attempts to edit a file via `edit_file` or `ast_replace`, Castor applies the diff in memory and immediately parses the rewritten buffer using `tree-sitter`. If the tree contains any `ERROR` or `UNDEFINED` syntax nodes, the write is aborted and an error is returned to the model with the exact parse failure coordinates. This guarantees that hallucinated syntax errors never corrupt the user's repository on disk.

### Q4: How does the 5-Layer Sandbox prevent escapes?
**A**: Castor enforces a defense-in-depth pipeline tested against 137 attack vectors:
1. **Layer 1: Lexical Normalizer** (strips null bytes, resolves tilde prefixes, collapses relative `.` and `..` dots).
2. **Layer 2: Canonical Root Guard** (locks execution to the canonicalized workspace root).
3. **Layer 3: Symlink Realpath Resolver** (resolves symlinks to their physical inode; aborts if the real path points outside the root).
4. **Layer 4: Out-of-Tree Refusal** (rejects any attempt to read, write, or list outside the workspace boundary).
5. **Layer 5: Binary File Sniffer** (inspects magic numbers to prevent dumping raw binary bytes into string buffers).
6. **Shell Lexical Scanner**: A regex/tokenizing security gate in `shell.rs` that blocks destructive commands (`rm -rf /`, `mkfs`, fork bombs, device writes, and sudo elevation) before execution.

---

## 2. Commercial, Pricing & ROI Questions

### Q5: "Claude Haiku 5.5 is $0.10 per million tokens. Why should I buy a GPU?"
**A**:
1. **Autonomous Volume**: Autocomplete uses 500 tokens. An autonomous agent uses 2M to 5M tokens per complex task. Over 60 days, our production telemetry ran **2.56 Billion tokens**. On Haiku, that would still have cost hundreds of dollars, and on Sonnet over $5,500. On Castor, local electricity cost **$9.07**.
2. **Regulatory & Egress Compliance**: Under DoD CMMC Level 2, ITAR, HIPAA, and client NDAs, sending proprietary code to third-party cloud APIs is illegal or contractually forbidden. Egress policy is a binary filter: if code cannot leave the building, cloud price cuts are completely irrelevant.
3. **Operating Safety**: Haiku provides raw text tokens, not a secured sandbox. Castor wraps local models in a 5-layer containment sandbox, an in-memory AST syntax gate, and a dynamic slot semaphore that prevents GPU out-of-memory crashes.

### Q6: What is the "Turnkey Swarm Setup Sprint"?
**A**: For software houses and dev teams that want to stop paying cloud API bills but lack in-house AI infrastructure engineers, we provide a **48-Hour Turnkey Setup Sprint ($1,500 one-time fixed fee)**. Our team visits on-site or connects remotely, tunes your office GPU workstation, installs Castor Swarm, integrates it with your private GitLab/GitHub server, and trains your developers. If it doesn't reduce your team's cloud API bills by at least 75% in 14 days, you receive a 100% refund.

### Q7: What are the commercial licensing options?
**A**:
- **Castor Community**: Open-source under AGPLv3 (free for individual developers and open-source projects).
- **Commercial Dual License**: For enterprise organizations or ISVs embedding Castor into proprietary, closed-source commercial software without AGPL copyleft obligations ($25,000/year base + $4.50/seat/month).
- **Enterprise Air-Gap Compliance**: Turnkey offline distribution with signed bundles, zero-egress legal attestation, and cATO compliance documentation ($25,000/year/enclave).

---

## 3. Compliance & Enterprise Security Questions

### Q8: Does Castor make any external network calls?
**A**: **Zero.** Castor's core MCP server communicates exclusively over standard input/output (stdio) or local loopback interfaces (`127.0.0.1`). It contains no telemetry beaconing, no auto-updater network calls, and no external crash reporter sockets. This has been validated by continuous packet capture and socket inspection tools (`netstat`, `ss`, `lsof`).

### Q9: How does Castor support DoD CMMC Level 2 & NIST SP 800-171?
**A**: Castor directly fulfills key CMMC Level 2 practices:
- **AC.L2-3.1.1 (Authorized Access)**: Confined within the workspace root boundary via our 5-layer path sandbox.
- **SI.L2-3.14.1 (Flaw Remediation)**: In-memory AST syntax validation prevents introduction of syntax defects.
- **AU.L2-3.3.1 (Audit Logging)**: Every tool call, AST modification, and command execution is recorded in an append-only, tamper-evident JSONL event ledger (`~/.castor/telemetry/`).
