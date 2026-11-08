# Getting Started

Install Castor, connect a local serving engine, and dispatch your first task.

---

## 1. Install & Register

Register Castor with every detected MCP client in one command:

```bash
# One-shot, no global install required:
npx -y mcp-castor install --client all

# …or install globally first:
npm install -g mcp-castor
castor install --client all
```

Prefer manual wiring? Drop this into your client's MCP config (`claude_desktop_config.json`, `~/.gemini/config/mcp_config.json`, or Cursor MCP settings):

```json
{
  "mcpServers": {
    "castor": {
      "command": "npx",
      "args": ["-y", "mcp-castor", "mcp"]
    }
  }
}
```

### Supported cloud clients

| Client | Registration |
|---|---|
| **Claude Code** | `castor install --client claude` |
| **Google Antigravity** | `castor install --client antigravity` (writes `~/.gemini/config/mcp_config.json`) |
| **Claude Desktop** | `claude_desktop_config.json` (manual or via installer) |
| **Cursor** | Cursor MCP configuration |
| **Any MCP client** | stdio MCP — standard JSON server configuration |

---

## 2. Connect Your Local Engine

Castor auto-discovers any OpenAI-compatible endpoint at `http://127.0.0.1:18020/v1` by probing
`/v1/models` to detect the model name, context window (`max_model_len`), and engine backend
(vLLM, llama.cpp, LM Studio, SGLang, Ollama) automatically. No config file or environment variables needed.

For custom endpoints or non-default ports, set `CASTOR_BASE_URL`:

```bash
# LM Studio (port 1234)
export CASTOR_BASE_URL="http://127.0.0.1:1234/v1"

# vLLM (custom port)
export CASTOR_BASE_URL="http://127.0.0.1:8000/v1"

# SGLang / Ollama / any other OpenAI-compatible /v1
export CASTOR_BASE_URL="http://127.0.0.1:<port>/v1"
```

See [llama-cpp.md](llama-cpp.md) for managed `llama-server` startup/shutdown and
Windows/Linux examples, and [engine-profiles.md](engine-profiles.md) for named profiles.
Configuration persists to `~/.castor/config.json`; environment variables always win.

### Supported serving engines

| Engine | Default Port | Notes |
|---|---|---|
| **vLLM** | 18020 | Auto-detected via `/v1/models` (`owned_by: "vllm"`); `guided_choice` DGI probe |
| **llama.cpp** (`llama-server`) | 18020 | Auto-detected via `/v1/models`; `--jinja` tool calling; managed start/stop via `castor server` |
| **LM Studio** | 1234 | Configure via `CASTOR_BASE_URL` |
| **SGLang** | 30000 | Configure via `CASTOR_BASE_URL` |
| **Ollama** | 11434 | Configure via `CASTOR_BASE_URL` |
| **Any OpenAI-compatible endpoint** | any | Configure via `CASTOR_BASE_URL` |

Auto-detection queries `/v1/models` (model name, `max_model_len`) and server fingerprints (`/health`, `/version`, `owned_by`) to apply optimal engine profiles without manual configuration.

---

## 3. Pair Program at $0

Prompt your cloud agent:

> *"Refactor auth middleware to use JWT and verify with `cargo test` using castor."*

The cloud architect plans. Castor executes the edit-and-test loop against the local model
in-tree at **$0 cloud token cost**. Only concise, verified deliverables return to the cloud.

---

## Next

- [Architecture](architecture.md): how the microkernel, DGI, semaphore, and proxy work.
- [Security](security.md): the zero-trust workspace containment pipeline.
- [CLI & Development](cli.md): command reference, building, and the test gate.
