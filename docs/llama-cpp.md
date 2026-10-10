# llama.cpp serving engine

Castor supports the OpenAI-compatible API exposed by `llama-server`, including
streaming chat, function calls, model-based dispatch classification, and managed
startup/shutdown on native Windows and Linux. Select this backend with
`CASTOR_ENGINE_TYPE=llama.cpp` or `"engine_type": "llama.cpp"` in config.json.
An unset or different engine type keeps the existing serving-engine behavior.

Install llama.cpp yourself and obtain a GGUF model with a suitable chat template.
Castor does not install binaries or download models. See the upstream
[server guide](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md)
and [function-calling guide](https://github.com/ggml-org/llama.cpp/blob/master/docs/function-calling.md).

## External server

Launch a foreground server in one terminal. This CPU example uses relative paths:

```sh
llama-server -m ./models/coder.gguf --host 127.0.0.1 --port 18020 \
  --alias castor-coder --jinja -c 8192 -np 1 -ngl 0
```

With a GPU-enabled llama.cpp build, replace `-ngl 0` with `-ngl 99` to offload
layers, or choose a layer count that fits your device. Model quantization and
context size affect memory requirements. Choose `-c` and `-np` together so each
slot has enough context for the task; configure `CASTOR_MAX_CONTEXT` to the
effective context available to each task. Hardware acceleration is a llama.cpp
build/runtime concern, not a different Castor backend.

In a second Linux terminal:

```sh
export CASTOR_ENGINE_TYPE=llama.cpp
export CASTOR_BASE_URL=http://127.0.0.1:18020/v1
export CASTOR_MODEL=castor-coder
export CASTOR_PORT_ENGINE=18020
castor server status
castor mcp
```

The corresponding PowerShell configuration is:

```powershell
$env:CASTOR_ENGINE_TYPE = 'llama.cpp'
$env:CASTOR_BASE_URL = 'http://127.0.0.1:18020/v1'
$env:CASTOR_MODEL = 'castor-coder'
$env:CASTOR_PORT_ENGINE = '18020'
castor server status
castor mcp
```

For Windows, launch `llama-server.exe` with the same arguments on one line;
use paths relative to that terminal's working directory. `--alias` must match
`CASTOR_MODEL`. Explicitly configure the API URL and model: these settings do
not acquire llama.cpp-specific defaults. The proxy forwards to the loopback
`CASTOR_PORT_ENGINE` port; when using it, keep that port aligned with llama-server.
An externally managed endpoint may also use a remote API base URL, but the local
stream proxy remains a loopback forwarding service.

Castor checks `<base_url>/health` and `<base_url>/models`, including any API
prefix. Loading-model HTTP 503 responses remain pending during managed startup.
Status and failed startup report connection, HTTP, JSON, and model-alias errors.
For a separately launched server, stop it in its owning terminal or configure an
explicit `stop_command`. Castor refuses to stop an external server merely because
it listens at the configured URL.

## Managed server

Use the same settings plus a launch command. Linux:

```sh
export CASTOR_LAUNCH_COMMAND='exec llama-server -m ./models/coder.gguf --host 127.0.0.1 --port 18020 --alias castor-coder --jinja -c 8192 -np 1'
castor server start
castor server status
castor server stop
```

PowerShell:

```powershell
$env:CASTOR_LAUNCH_COMMAND = 'llama-server.exe -m ./models/coder.gguf --host 127.0.0.1 --port 18020 --alias castor-coder --jinja -c 8192 -np 1'
castor server start
castor server status
castor server stop
```

Quote executable and model paths inside the command when they contain spaces.
Commands use `sh -c` on Linux and `cmd /c` on Windows. Keep the server in the
foreground; do not daemonize it or use `start`, `nohup`, or a trailing `&`.
Relative launch paths resolve from the Castor caller's working directory.

Equivalent persistent configuration at `$CASTOR_STATE_DIR/config.json` (or the
default Castor state directory) is:

```json
{
  "engine_type": "llama.cpp",
  "base_url": "http://127.0.0.1:18020/v1",
  "model": "castor-coder",
  "launch_command": "llama-server -m ./models/coder.gguf --host 127.0.0.1 --port 18020 --alias castor-coder --jinja -c 8192 -np 1",
  "ports": { "engine": 18020 }
}
```

Environment variables override the corresponding JSON settings. Windows users
can substitute `llama-server.exe` in the JSON launch command. Keep the same state
directory, endpoint, and model across start/stop invocations.

A detached Castor supervisor owns the foreground child, retains a bounded stderr
tail, and exposes authenticated control on an ephemeral loopback port. Its atomic
ownership record contains PID, OS process-birth identity, API endpoint, model, and
control credentials. A subsequent CLI or MCP invocation verifies that record
before requesting shutdown. Start and stop share a cross-process lifecycle lock.
Startup failure stops the owned child and reports stderr and readiness diagnostics.
An explicit `stop_command` takes precedence over managed shutdown; it must target
only the intended server and keep ownership consistent with its actions.

Stale or mismatched ownership records cause an error. Inspect the recorded process
and stop any surviving server manually before removing a stale `llama-owner.json`
from your state directory. Never change a recorded PID to another running process.
Keep the state directory private: the ownership file contains control credentials.

Run Castor and llama-server together inside WSL for WSL-managed operation.
Native Windows Castor does not manage Linux process ownership through `wsl.exe`.
Use separate state directories for native Windows and WSL deployments.

## Tools, reasoning, and images

Function calling requires a tool-capable GGUF model and Jinja chat template.
Use `--jinja`; if the GGUF template does not support tools, provide an appropriate
`--chat-template-file`. Castor uses OpenAI-style tool definitions, assistant tool
calls, and tool-result messages, including streamed argument fragments.

For this backend, Castor sends per-task reasoning effort in the top-level
`reasoning_effort` field. An absent effort leaves the template default untouched.
The dispatch classifier uses the model's chat template, a constrained JSON
verdict, and disabled thinking. Its failures produce a visible warning and the
existing advisory heuristic rather than an invented verdict.

Existing image attachments use OpenAI-style `image_url` messages. Vision requires
a supported multimodal model and its matching projector, configured with
`--mmproj ./models/projector.gguf` where required. Text-only models cannot inspect
images; do not use `--skip-chat-parsing` for tool workflows.

For authentication, start llama-server with its API-key configuration and set
`CASTOR_API_KEY` to the matching key. Store credentials outside version control;
Castor sends bearer authentication for chat, readiness, and dispatch classification.

## Verification

The normal tests use ephemeral-port HTTP fixtures and harmless local processes:

```sh
cargo test --test llama_cpp
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

The real-model smoke test is ignored by default. It creates its own state
directory and selects an isolated loopback port, verifies streamed function calling
and the subsequent tool-result response, then stops only its owned server:

```sh
export ALLOW_ENGINE_INTERRUPT=1
export CASTOR_LLAMA_SERVER=./llama-cpp/build/bin/llama-server
export CASTOR_LLAMA_MODEL_FILE=./models/coder.gguf
cargo test --test llama_cpp live_llama_cpp_tool_round_trip -- --ignored --nocapture
```

Use PowerShell `$env:NAME = 'value'` assignments and a native `.exe` path on
Windows. Supply a model that supports function calling; this test does not
download one. Keep `ALLOW_ENGINE_INTERRUPT=0` for the ordinary offline gates.

To check a server that is already running, including a directly exposed Unsloth
llama.cpp server, use the attachment test instead. It checks readiness, model-based
classification, and a streamed tool round trip without launching or stopping the
server:

```sh
export ALLOW_ENGINE_INTERRUPT=1
export CASTOR_LLAMA_EXISTING_BASE_URL=http://127.0.0.1:18020/v1
export CASTOR_LLAMA_EXISTING_MODEL=castor-coder
cargo test --test llama_cpp live_existing_llama_cpp_tool_round_trip -- --ignored --nocapture
```

Provide `CASTOR_API_KEY` if needed. Point this test at the llama.cpp inference API,
which may have a different address from the Unsloth Studio user interface.
