//! Castor CLI commands, arguments parser, and execution handlers.

use clap::{Parser, Subcommand};

use crate::config;
use crate::engine;
use crate::evals::runner::{Outcome, Variant};
use crate::evo::lineage::Lineage;
use crate::evo::watchdog::WatchdogVerdict;
use crate::evo::{optimizer, watchdog};
use crate::mcp;
use crate::proxy;
use crate::pruner;
use crate::state;
use crate::task;
use crate::telemetry;

#[derive(Parser)]
#[command(
    name = "castor",
    version,
    about = "Castor: Rust MCP toolchain, proxy, and evo engine"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum EvoAction {
    /// Score the current artifact over the evals dir and commit it to the lineage
    Run {
        /// Override the evals dir (default: `<repo>/evals` if present)
        #[arg(long)]
        evals: Option<String>,
    },
    /// Show the lineage head / best parent / fitness and the watchdog verdict
    Status,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the MCP server (default subcommand)
    Mcp,
    /// Run the stream proxy
    Proxy {
        /// Override the state dir root
        #[arg(long)]
        state_dir: Option<String>,
        /// Override the listen port
        #[arg(long)]
        port: Option<u16>,
        /// Override the upstream engine port
        #[arg(long)]
        engine_port: Option<u16>,
    },
    /// Run the status / long-poll HTTP server (singleton)
    Status {
        /// Override the state dir root
        #[arg(long)]
        state_dir: Option<String>,
        /// Override the listen port
        #[arg(long)]
        port: Option<u16>,
    },
    /// Manage the engine (status | start | stop)
    Server {
        /// Action: status, start, or stop
        action: String,
    },
    /// Manage configuration
    Config,
    /// Install / register the Castor MCP server with a client
    Install {
        /// Target client: claude, antigravity, or all
        #[arg(long, default_value = "all")]
        client: String,
    },
    /// Clean state
    Clean {
        /// Actually delete (default: dry run, print the plan only)
        #[arg(long)]
        yes: bool,
    },
    /// View telemetry and operational statistics
    Stats {
        /// Print machine-readable JSON
        #[arg(short, long)]
        json: bool,
        /// Restrict stats to events within a duration of now
        /// (e.g. `24h`, `7d`, `120m`)
        #[arg(short, long, value_name = "DURATION")]
        since: Option<String>,
        /// Break down the stats by UTC day (`YYYY-MM-DD`)
        #[arg(short = 'd', long)]
        by_day: bool,
        /// Disable colored output
        #[arg(long)]
        no_color: bool,
    },
    /// Evo engine
    Evo {
        #[command(subcommand)]
        action: EvoAction,
    },
    /// (test helper) hold a task slot for a few seconds
    #[command(name = "__sem_child", hide = true)]
    __SemChild {
        /// State dir root
        state_dir: String,
        /// Task id to bind to the lease
        task_id: String,
    },
    /// (internal) background worker process for a dispatched task
    #[command(name = "__worker", hide = true)]
    __Worker {
        /// Path to the JSON JobSpec file
        spec_path: String,
    },
}

/// Build a human-readable plan listing what *would* be deleted.
fn format_plan(plan: &pruner::PrunePlan) -> String {
    if plan.is_empty() {
        return "nothing to clean\n".to_string();
    }
    let mut out = String::new();
    for p in &plan.sessions {
        out.push_str(&format!("session   {}\n", p.display()));
    }
    for p in &plan.tasks {
        out.push_str(&format!("task      {}\n", p.display()));
    }
    for p in &plan.telemetry {
        out.push_str(&format!("telemetry {}\n", p.display()));
    }
    for p in &plan.evo {
        out.push_str(&format!("evo       {}\n", p.display()));
    }
    out
}

/// Run the `clean` subcommand against `state_dir`.
///
/// Without `yes`: build a default policy, compute the plan, and return the
/// plan text (nothing is deleted). With `yes`: apply the plan and return one
/// line per deletion. Errors are returned as a typed message string.
pub fn run_clean(state_dir: &std::path::Path, yes: bool) -> Result<String, String> {
    let policy = pruner::PrunePolicy::default();
    let mut plan = pruner::plan(state_dir, &policy);

    if !yes {
        return Ok(format_plan(&plan));
    }

    let deleted = pruner::apply(state_dir, &mut plan).map_err(|e| e.to_string())?;

    let mut out = String::new();
    for p in &plan.sessions {
        out.push_str(&format!("deleted session   {}\n", p.display()));
    }
    for p in &plan.tasks {
        out.push_str(&format!("deleted task      {}\n", p.display()));
    }
    for p in &plan.telemetry {
        out.push_str(&format!("deleted telemetry {}\n", p.display()));
    }
    for p in &plan.evo {
        out.push_str(&format!("deleted evo       {}\n", p.display()));
    }
    out.push_str(&format!("deleted {deleted} item(s)\n"));
    Ok(out)
}

/// The lineage file location under the state dir.
fn evo_lineage_path(state: &state::StateDir) -> std::path::PathBuf {
    state.evo().join("lineage.jsonl")
}

/// Resolve the evals dir: an explicit override, else the repo's `evals/`
/// (checked relative to the current dir, then one level up for the crate
/// layout). A missing dir is a typed, actionable error.
fn resolve_evals_dir(override_dir: Option<&str>) -> Result<std::path::PathBuf, String> {
    if let Some(dir) = override_dir {
        let p = std::path::PathBuf::from(dir);
        if p.is_dir() {
            return Ok(p);
        }
        return Err(format!(
            "evo: evals dir not found: {} (pass a valid --evals path)",
            p.display()
        ));
    }
    for candidate in ["evals", "../evals"] {
        let p = std::path::PathBuf::from(candidate);
        if p.is_dir() {
            return Ok(p);
        }
    }
    Err(
        "evo: no evals dir found (looked for ./evals and ../evals); \
         pass --evals <dir> pointing at a dir of task subdirectories"
            .to_string(),
    )
}

/// Run the `evo run` subcommand: score the current artifact over the evals
/// dir, commit it into `<state>/evo/lineage.jsonl`, and return the report
/// text to print.
pub fn run_evo(evals_override: Option<&str>, state: &state::StateDir) -> Result<String, String> {
    let evals_dir = resolve_evals_dir(evals_override)?;
    let report =
        optimizer::score_artifact(&evals_dir, Variant::Golden).map_err(|e| format!("evo: {e}"))?;

    let path = evo_lineage_path(state);
    let mut lineage = Lineage::load(&path).map_err(|e| format!("evo: {e}"))?;
    let now_ms = now_epoch_ms();
    let artifact_ref = format!("artifact://{}@{}", evals_dir.display(), now_ms);
    let id = optimizer::commit_artifact(&mut lineage, artifact_ref, &report, now_ms)
        .map_err(|e| format!("evo: {e}"))?;
    lineage.save().map_err(|e| format!("evo: {e}"))?;

    let mut out = String::new();
    out.push_str(&format!("evo run: evals={}\n", evals_dir.display()));
    for e in &report.entries {
        let (label, fitness) = match &e.outcome {
            Outcome::Pass => ("pass", "1.0".to_string()),
            Outcome::Fail(_) => ("fail", "0.0".to_string()),
            Outcome::Error(reason) => {
                out.push_str(&format!("  {}  error  ({reason})\n", e.task_id));
                continue;
            }
        };
        out.push_str(&format!("  {}  {}  fitness={fitness}\n", e.task_id, label));
    }
    let mean = report
        .mean_fitness()
        .map(|f| format!("{f:.4}"))
        .unwrap_or_else(|| "n/a (no scoreable tasks)".to_string());
    out.push_str(&format!("mean fitness: {mean}\n"));
    out.push_str(&format!("committed: {id} -> {}\n", path.display()));
    Ok(out)
}

/// Run the `evo status` subcommand: load the lineage and return the report
/// text (head / best parent / fitness + the watchdog verdict).
pub fn evo_status(state: &state::StateDir) -> Result<String, String> {
    let path = evo_lineage_path(state);
    let lineage = Lineage::load(&path).map_err(|e| format!("evo: {e}"))?;

    let mut out = String::new();
    out.push_str(&format!("evo status: lineage={}\n", path.display()));
    out.push_str(&format!("nodes: {}\n", lineage.nodes().len()));

    if lineage.nodes().is_empty() {
        out.push_str("verdict: empty (no commits yet)\n");
        return Ok(out);
    }

    let head = lineage.head();
    let best = lineage.best_parent();
    out.push_str(&format!(
        "head: {}\n",
        head.map(|n| format!("{} (fitness={})", n.id, fmt_fitness(n.fitness)))
            .unwrap_or_else(|| "n/a (no scored leaf)".to_string())
    ));
    out.push_str(&format!(
        "best_parent: {}\n",
        best.map(|n| format!("{} (fitness={})", n.id, fmt_fitness(n.fitness)))
            .unwrap_or_else(|| "n/a (no scored node)".to_string())
    ));

    let verdict = watchdog::evaluate(&lineage, now_epoch_ms());
    out.push_str(&format!("watchdog: {}\n", fmt_verdict(&verdict)));
    Ok(out)
}

fn fmt_fitness(f: Option<f64>) -> String {
    f.map(|x| format!("{x:.4}"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn fmt_verdict(v: &WatchdogVerdict) -> String {
    match v {
        WatchdogVerdict::Empty => "empty (no commits yet)".to_string(),
        WatchdogVerdict::Healthy => "healthy".to_string(),
        WatchdogVerdict::StalledOld => "stalled (newest commit older than 7 days)".to_string(),
        WatchdogVerdict::StalledNoImprovement => {
            "stalled (no fitness improvement in the last 5 commits)".to_string()
        }
    }
}

/// Current wall-clock time as epoch milliseconds.
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run the `server` subcommand against the engine lifecycle.
///
/// `status` runs the canary and returns one honest line (healthy: the
/// configured model id is listed; unhealthy: the verbatim reason). `start`
/// boots the engine (an already-healthy engine is a success short-circuit,
/// printed as such). `stop` stops it. Returns the line to print plus whether
/// the action succeeded (drives the exit code). Errors are returned as a
/// typed message string.
pub async fn run_server(
    action: &str,
    config: &config::Config,
    state: &state::StateDir,
) -> Result<(String, bool), String> {
    let lc = engine::EngineLifecycle::new(config, state);
    match action {
        "status" => {
            let healthy = match &config.model {
                Some(_) => lc.canary().await,
                None => false,
            };
            let line = if healthy {
                format!("healthy: model {} listed", config.model.as_deref().unwrap())
            } else if config.model.is_none() {
                "unhealthy: no model configured".to_string()
            } else {
                "unhealthy: canary failed (engine not healthy)".to_string()
            };
            Ok((line, healthy))
        }
        "start" => match lc.boot().await {
            Ok(()) => Ok(("engine booted".to_string(), true)),
            Err(engine::LifecycleError::AlreadyRunning) => {
                Ok(("engine already running".to_string(), true))
            }
            Err(e) => Err(format!("server: {e}")),
        },
        "stop" => match lc.stop().await {
            Ok(()) => Ok(("engine stopped".to_string(), true)),
            Err(e) => Err(format!("server: {e}")),
        },
        other => Err(format!(
            "server: unknown action '{other}' (expected status, start, or stop)"
        )),
    }
}

/// Default config file location for a client (mirrors the JS reference:
/// Antigravity uses `~/.gemini/config/mcp_config.json`, Claude Code uses
/// `~/.claude.json`).
fn default_config_path(client: &str) -> std::path::PathBuf {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(std::path::PathBuf::from)
    } else {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }
    .unwrap_or_else(|| std::path::PathBuf::from("."));
    match client {
        "claude" => home.join(".claude.json"),
        "antigravity" => home.join(".gemini").join("config").join("mcp_config.json"),
        _ => unreachable!("client is validated before this is called"),
    }
}

/// The `castor mcp` server spec merged into the client's config JSON.
///
/// The command is the current executable (or `node index.js`-style dev
/// invocation is intentionally NOT used here — this is the Rust binary),
/// so the registration is self-contained and npm-free.
fn castor_server_spec() -> serde_json::Value {
    let command = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "castor".to_string());
    serde_json::json!({ "command": command, "args": ["mcp"] })
}

/// Merge the Castor MCP registration into a client config file, idempotently.
///
/// Reads the JSON object at `path` (missing file → empty object; corrupt
/// file → start fresh, matching the JS reference), sets
/// `mcpServers.castor` to the server spec (existing entry is updated in
/// place, sibling servers and all other top-level keys are preserved), and
/// writes the result back atomically (temp file + rename). Returns a
/// one-line summary of what was written.
fn merge_into_config(path: &std::path::Path) -> Result<String, String> {
    let mut cfg = serde_json::Map::new();
    if path.exists() {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("install: cannot read {}: {e}", path.display()))?;
        let trimmed = raw.trim_start_matches('\u{feff}');
        match serde_json::from_str::<serde_json::Value>(trimmed) {
            Ok(serde_json::Value::Object(obj)) => cfg = obj,
            Ok(_) => {
                eprintln!(
                    "install: {} is not a JSON object; starting fresh",
                    path.display()
                );
            }
            Err(e) => {
                eprintln!("install: corrupt {}: {e}; starting fresh", path.display());
            }
        }
    }

    let spec = castor_server_spec();
    let servers = cfg
        .entry("mcpServers")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !servers.is_object() {
        *servers = serde_json::Value::Object(serde_json::Map::new());
    }
    let servers = servers.as_object_mut().unwrap();
    servers.insert("castor".to_string(), spec);
    let names: Vec<String> = servers.keys().cloned().collect();

    let out = serde_json::to_string_pretty(&serde_json::Value::Object(cfg))
        .map_err(|e| format!("install: cannot serialize config: {e}"))?
        + "\n";

    // Atomic write: temp file in the same directory, then rename.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("install: cannot create {}: {e}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &out)
        .map_err(|e| format!("install: cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        format!(
            "install: cannot rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        )
    })?;

    Ok(format!(
        "install: merged mcpServers.castor into {} (servers: {})",
        path.display(),
        names.join(", ")
    ))
}

/// Run the `install` subcommand.
///
/// `client` is `claude`, `antigravity`, or `all`. For each target client the
/// `castor mcp` registration entry is merged into that client's config JSON
/// (idempotent: an existing `mcpServers.castor` entry is updated in place,
/// sibling servers and other keys are untouched, the file is written
/// atomically via temp+rename, a missing file is created).
///
/// `config_path_override` replaces the default config path for ALL targets
/// (test hook — no globals). Returns the lines to print, one per target.
pub fn run_install(
    client: &str,
    config_path_override: Option<&std::path::Path>,
) -> Result<String, String> {
    let targets: Vec<&str> = match client {
        "claude" => vec!["claude"],
        "antigravity" => vec!["antigravity"],
        "all" => vec!["antigravity", "claude"],
        other => {
            return Err(format!(
                "install: unknown client '{other}' (expected claude, antigravity, or all)"
            ));
        }
    };

    let mut out = String::new();
    for t in targets {
        let path = config_path_override
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| default_config_path(t));
        let line = merge_into_config(&path)?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

/// Run the CLI application from command-line arguments.
pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    run_with(cli).await
}

/// Run the CLI application with a pre-parsed `Cli` structure.
pub async fn run_with(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command.unwrap_or(Command::Mcp) {
        Command::Mcp => {
            let loaded = config::load().map_err(|e| format!("config: {e}"))?;
            // Best-effort, non-blocking bring-up of the status server
            // (lock → probe → spawn if down → release). Never blocks serve.
            let state = state::StateDir::from_config(&loaded.config);
            let _ = state.ensure();
            task::wait::ensure_status_server(&state, loaded.config.ports.status).await;
            // Best-effort, non-blocking bring-up of the stream proxy.
            proxy::ensure_proxy_server(
                &state,
                loaded.config.ports.proxy,
                loaded.config.ports.engine,
            )
            .await;
            mcp::serve(&loaded.config.tool_prefix).await?;
        }
        Command::Proxy {
            state_dir,
            port,
            engine_port,
        } => {
            let loaded = config::load().map_err(|e| format!("config: {e}"))?;
            let state = match state_dir {
                Some(dir) => state::StateDir::new(dir),
                None => state::StateDir::from_config(&loaded.config),
            };
            let _ = state.ensure();
            let port = port.unwrap_or(loaded.config.ports.proxy);
            let engine_port = engine_port.unwrap_or(loaded.config.ports.engine);
            let upstream = std::net::SocketAddr::from(([127, 0, 0, 1], engine_port));
            let server = proxy::ProxyServer::new(&state, upstream);
            if !server.try_acquire_lock() {
                // Another live process holds the singleton lock: exit 0 with a
                // single stderr line (never fight a live keeper).
                eprintln!("proxy already running");
                return Ok(());
            }
            server.serve(port).await?;
        }
        Command::Status { state_dir, port } => {
            let loaded = config::load().map_err(|e| format!("config: {e}"))?;
            let state = match state_dir {
                Some(dir) => state::StateDir::new(dir),
                None => state::StateDir::from_config(&loaded.config),
            };
            let _ = state.ensure();
            let port = port.unwrap_or(loaded.config.ports.status);
            let server = task::wait::StatusServer::new(&state);
            if !server.try_acquire_lock() {
                // Another live process holds the singleton lock: exit 0 with a
                // single stderr line (never fight a live keeper).
                eprintln!("status server already running");
                return Ok(());
            }
            server.serve(port).await?;
        }
        Command::Server { action } => {
            let loaded = config::load().map_err(|e| format!("config: {e}"))?;
            let state = state::StateDir::from_config(&loaded.config);
            let _ = state.ensure();
            match run_server(&action, &loaded.config, &state).await {
                Ok((text, ok)) => {
                    println!("{text}");
                    if !ok {
                        std::process::exit(1);
                    }
                }
                Err(msg) => {
                    eprintln!("castor: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Command::Config => {
            if let Err(e) = config::print_effective() {
                eprintln!("castor: {e}");
                std::process::exit(1);
            }
        }
        Command::Install { client } => match run_install(&client, None) {
            Ok(text) => print!("{text}"),
            Err(msg) => {
                eprintln!("castor: {msg}");
                std::process::exit(1);
            }
        },
        Command::Clean { yes } => {
            let loaded = config::load().map_err(|e| format!("config: {e}"))?;
            let state = state::StateDir::from_config(&loaded.config);
            let _ = state.ensure();
            match run_clean(state.root(), yes) {
                Ok(text) => print!("{text}"),
                Err(msg) => {
                    eprintln!("castor: {msg}");
                    std::process::exit(1);
                }
            }
        }
        Command::Stats {
            json,
            since,
            by_day,
            no_color,
        } => {
            let loaded = config::load().map_err(|e| format!("config: {e}"))?;
            let state = state::StateDir::from_config(&loaded.config);
            let mut opts = telemetry::StatsOptions {
                by_day,
                ..Default::default()
            };
            if let Some(s) = &since {
                let duration = humantime::parse_duration(s).map_err(|e| {
                    format!(
                        "stats: invalid --since duration '{s}': {e} \
                         (expected a duration like 24h, 7d, 30m)"
                    )
                })?;
                let since_ms = now_epoch_ms() as i128 - duration.as_secs_f64() as i128 * 1000;
                opts.since_ms = Some(since_ms);
            }
            let stats = telemetry::derive_stats(state.root(), &opts);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&stats).map_err(|e| e.to_string())?
                );
            } else {
                use std::io::IsTerminal;
                let use_color = !no_color
                    && std::io::stdout().is_terminal()
                    && std::env::var("NO_COLOR").is_err();
                print!("{}", telemetry::format_stats_card_styled(&stats, use_color));
            }
        }
        Command::Evo { action } => {
            let loaded = config::load().map_err(|e| format!("config: {e}"))?;
            let state = state::StateDir::from_config(&loaded.config);
            let _ = state.ensure();
            // `run_task` builds its own current-thread tokio runtime, so the
            // evo work must run outside the `#[tokio::main]` runtime. We
            // spawn a plain std thread for that.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result = match action {
                    EvoAction::Run { evals } => run_evo(evals.as_deref(), &state),
                    EvoAction::Status => evo_status(&state),
                };
                let _ = tx.send(result);
            });
            match rx.recv() {
                Ok(Ok(text)) => print!("{text}"),
                Ok(Err(msg)) => {
                    eprintln!("castor: {msg}");
                    std::process::exit(1);
                }
                Err(_) => {
                    eprintln!("castor: evo worker thread panicked");
                    std::process::exit(1);
                }
            }
        }
        Command::__SemChild { state_dir, task_id } => {
            let state = state::StateDir::new(state_dir);
            let _ = state.ensure();
            let sem = task::semaphore::TaskSemaphore::new(&state, 1);
            let lease = sem.acquire(&task_id).await;
            println!("HELD");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let _ = sem.release(&lease);
        }
        Command::__Worker { spec_path } => {
            let loaded = config::load().map_err(|e| format!("config: {e}"))?;
            let state = state::StateDir::from_config(&loaded.config);
            let _ = state.ensure();
            let raw = std::fs::read_to_string(&spec_path)
                .map_err(|e| format!("cannot read job spec {spec_path}: {e}"))?;
            let spec: mcp::worker::JobSpec = serde_json::from_str(&raw)
                .map_err(|e| format!("invalid job spec JSON {spec_path}: {e}"))?;
            mcp::worker::run_job(&spec, &state, &loaded.config)
                .await
                .map_err(|e| format!("worker failed: {e}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_subcommand_is_mcp() {
        let cli = Cli::parse_from(["castor"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn all_subcommands_parse() {
        for name in [
            "mcp", "proxy", "status", "server", "config", "install", "clean", "stats", "evo",
        ] {
            let args: Vec<&str> = match name {
                "server" => vec!["castor", "server", "status"],
                "evo" => vec!["castor", "evo", "status"],
                _ => vec!["castor", name],
            };
            let cli = Cli::parse_from(args);
            assert!(cli.command.is_some(), "subcommand {name} should parse");
        }
    }

    #[test]
    fn server_parses_action() {
        let cli = Cli::parse_from(["castor", "server", "start"]);
        assert!(matches!(
            cli.command,
            Some(Command::Server {
                action
            }) if action == "start"
        ));
    }

    #[test]
    fn clean_parses_yes_flag() {
        let cli = Cli::parse_from(["castor", "clean", "--yes"]);
        assert!(matches!(cli.command, Some(Command::Clean { yes: true })));
        let cli = Cli::parse_from(["castor", "clean"]);
        assert!(matches!(cli.command, Some(Command::Clean { yes: false })));
    }

    // --- run_clean behavior tests -------------------------------------------

    use std::io::Write;
    use std::time::{Duration, SystemTime};

    static CLEAN_TMP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn clean_tmp_dir() -> std::path::PathBuf {
        let n = CLEAN_TMP.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("castor-clean-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn set_mtime(path: &std::path::Path, t: SystemTime) {
        let times = std::fs::FileTimes::new().set_modified(t);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
            const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
            if let Ok(f) = std::fs::OpenOptions::new()
                .access_mode(FILE_WRITE_ATTRIBUTES)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                .open(path)
            {
                let _ = f.set_times(times);
            }
        }
        #[cfg(not(windows))]
        {
            if let Ok(f) = std::fs::File::open(path) {
                let _ = f.set_times(times);
            }
        }
    }

    /// A session dir with a terminal event, mtime 30 days old (past 14-day
    /// retention) so the default policy prunes it.
    fn make_old_session(root: &std::path::Path, id: &str) -> std::path::PathBuf {
        let dir = root.join("sessions").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        let mut f = std::fs::File::create(&events).unwrap();
        writeln!(f, r#"{{"type":"session_start"}}"#).unwrap();
        writeln!(f, r#"{{"type":"session_end"}}"#).unwrap();
        f.flush().unwrap();
        drop(f);
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
        set_mtime(&events, old);
        set_mtime(&dir, old);
        dir
    }

    #[test]
    fn run_clean_dry_run_deletes_nothing() {
        let root = clean_tmp_dir();
        let _ = std::fs::create_dir_all(root.join("tasks").join("slots"));
        let _ = std::fs::create_dir_all(root.join("telemetry"));
        let _ = std::fs::create_dir_all(root.join("locks"));
        let _ = std::fs::create_dir_all(root.join("evo"));
        let session = make_old_session(&root, "s_old");

        let out = run_clean(&root, false).expect("dry run should succeed");
        // Plan text names the old session.
        assert!(
            out.contains("s_old"),
            "plan should list the old session: {out}"
        );
        // Nothing was deleted.
        assert!(session.exists(), "dry run must not delete the session");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_clean_yes_deletes() {
        let root = clean_tmp_dir();
        let _ = std::fs::create_dir_all(root.join("tasks").join("slots"));
        let _ = std::fs::create_dir_all(root.join("telemetry"));
        let _ = std::fs::create_dir_all(root.join("locks"));
        let _ = std::fs::create_dir_all(root.join("evo"));
        let session = make_old_session(&root, "s_old");

        let out = run_clean(&root, true).expect("apply should succeed");
        // One line per deletion, plus the summary.
        assert!(
            out.contains("deleted session"),
            "should report the deletion: {out}"
        );
        assert!(
            out.contains("s_old"),
            "deletion line should name the session: {out}"
        );
        // The session is actually gone.
        assert!(!session.exists(), "apply must delete the session");
        let _ = std::fs::remove_dir_all(&root);
    }

    // --- run_evo / evo_status behavior tests ---------------------------------

    static EVO_TMP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn evo_tmp_dir() -> std::path::PathBuf {
        let n = EVO_TMP.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("castor-evo-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Write a minimal passing task dir (same pattern as optimizer tests).
    fn write_pass_task(task_dir: &std::path::Path) {
        std::fs::create_dir_all(task_dir.join("fixture")).unwrap();
        std::fs::write(
            task_dir.join("task.toml"),
            "id = \"t1\"\nprompt = \"p\"\ntimeout_s = 10\ntags = []\n[scorer]\nchecks = []\n",
        )
        .unwrap();
        std::fs::write(
            task_dir.join("trace.jsonl"),
            "{\"timestamp\":\"t\",\"sessionId\":\"s\",\"type\":\"assistant_message\",\"content\":\"done\",\"toolCalls\":[]}\n",
        )
        .unwrap();
    }

    #[test]
    fn run_evo_appends_node_and_prints_fitness() {
        let root = evo_tmp_dir();
        let state = state::StateDir::new(&root);
        let _ = state.ensure();

        // Temp evals dir with one passing task.
        let evals = root.join("evals");
        std::fs::create_dir_all(&evals).unwrap();
        write_pass_task(&evals.join("t1"));

        let out = run_evo(Some(&evals.to_string_lossy()), &state).expect("run_evo should succeed");
        assert!(out.contains("mean fitness: 1.0000"), "{out}");
        assert!(out.contains("committed:"), "{out}");

        // The lineage file now has one node.
        let lin = Lineage::load(&evo_lineage_path(&state)).unwrap();
        assert_eq!(lin.nodes().len(), 1);
        assert_eq!(lin.nodes()[0].fitness, Some(1.0));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn evo_status_empty_lineage() {
        let root = evo_tmp_dir();
        let state = state::StateDir::new(&root);
        let _ = state.ensure();

        let out = evo_status(&state).expect("status should succeed");
        assert!(out.contains("nodes: 0"), "{out}");
        assert!(out.contains("verdict: empty"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn evo_status_stalled_old() {
        let root = evo_tmp_dir();
        let state = state::StateDir::new(&root);
        let _ = state.ensure();

        // Fabricate a lineage with a very old commit.
        let path = evo_lineage_path(&state);
        std::fs::write(
            &path,
            "{\"id\":\"a\",\"parents\":[],\"fitness\":1.0,\"artifact_ref\":\"x\",\"created_at\":\"2020-01-01T00:00:00.000Z\"}\n",
        )
        .unwrap();

        let out = evo_status(&state).expect("status should succeed");
        assert!(out.contains("stalled"), "{out}");
        assert!(out.contains("older than 7 days"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn evo_status_stalled_no_improvement() {
        let root = evo_tmp_dir();
        let state = state::StateDir::new(&root);
        let _ = state.ensure();

        // Fabricate 5 recent commits (within the last day, so the
        // StalledOld rule cannot fire) with the same fitness (no improvement).
        let now = now_epoch_ms();
        let mut lines = String::new();
        for i in 0..5 {
            let parents = if i == 0 {
                "[]".to_string()
            } else {
                format!("[\"n{}\"]", i - 1)
            };
            let created = optimizer::iso8601_utc(now - (5 - i) * 3_600_000);
            lines.push_str(&format!(
                "{{\"id\":\"n{}\",\"parents\":{},\"fitness\":0.5,\"artifact_ref\":\"x\",\"created_at\":\"{}\"}}\n",
                i, parents, created
            ));
        }
        let path = evo_lineage_path(&state);
        std::fs::write(&path, &lines).unwrap();

        let out = evo_status(&state).expect("status should succeed");
        assert!(out.contains("stalled"), "{out}");
        assert!(out.contains("no fitness improvement"), "{out}");

        let _ = std::fs::remove_dir_all(&root);
    }

    // --- run_server behavior tests ------------------------------------------

    /// Mock engine: /v1/models returns 200 listing the given model ids.
    /// Binds to an ephemeral port (never the real engine port).
    async fn start_models_mock(models: Vec<String>) -> u16 {
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(move || {
                let models = models.clone();
                async move {
                    axum::Json(serde_json::json!({
                        "data": models.iter().map(|id| serde_json::json!({"id": id})).collect::<Vec<_>>()
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    fn server_test_config(state: &state::StateDir, port: u16) -> config::Config {
        config::Config {
            model: Some("test-model".into()),
            base_url: None,
            api_key: None,
            engine_type: None,
            launch_command: None,
            stop_command: None,
            max_context: None,
            ports: config::Ports {
                engine: port,
                status: 0,
                proxy: 0,
            },
            max_concurrent_tasks: 1,
            tool_prefix: String::new(),
            searxng_url: None,
            brave_api_key: None,
            openalex_email: None,
            openalex_api_key: None,
            boot_timeout_secs: 180,
            probe_budget: 4,
            state_dir: state.root().to_path_buf(),
        }
    }

    #[tokio::test]
    async fn run_server_status_reports_canary() {
        let state = state::StateDir::new(clean_tmp_dir());
        let _ = state.ensure();

        // Healthy: the mock lists the configured model.
        let port = start_models_mock(vec!["test-model".into()]).await;
        let cfg = server_test_config(&state, port);
        let (line, ok) = run_server("status", &cfg, &state).await.unwrap();
        assert_eq!(line, "healthy: model test-model listed");
        assert!(ok, "healthy status must exit 0");

        // Unhealthy: nothing listens on the engine port (offline, ephemeral).
        let cfg2 = server_test_config(&state, 1);
        let (line2, ok2) = run_server("status", &cfg2, &state).await.unwrap();
        assert_eq!(line2, "unhealthy: canary failed (engine not healthy)");
        assert!(!ok2, "unhealthy status must exit 1");

        // Unknown action is a typed error.
        let err = run_server("reboot", &cfg, &state).await.unwrap_err();
        assert!(err.contains("unknown action"), "{err}");

        let _ = std::fs::remove_dir_all(state.root());
    }

    // --- run_install behavior tests -----------------------------------------

    static INSTALL_TMP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn install_tmp_dir() -> std::path::PathBuf {
        let n = INSTALL_TMP.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("castor-install-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn install_creates_entry_in_missing_file() {
        let dir = install_tmp_dir();
        let cfg = dir.join("mcp_config.json");
        assert!(!cfg.exists());

        let out = run_install("antigravity", Some(&cfg)).expect("install should succeed");
        assert!(out.contains("mcp_config.json"), "{out}");

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let spec = parsed["mcpServers"]["castor"].clone();
        assert!(
            spec["command"].is_string(),
            "spec must have a command: {spec}"
        );
        assert_eq!(spec["args"], serde_json::json!(["mcp"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_is_idempotent() {
        let dir = install_tmp_dir();
        let cfg = dir.join("mcp_config.json");

        let first = run_install("antigravity", Some(&cfg)).unwrap();
        let bytes1 = std::fs::read(&cfg).unwrap();

        let second = run_install("antigravity", Some(&cfg)).unwrap();
        let bytes2 = std::fs::read(&cfg).unwrap();

        assert_eq!(first, second, "output lines must be identical");
        assert_eq!(bytes1, bytes2, "second install must be byte-identical");

        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8_lossy(&bytes2)).unwrap();
        let servers = parsed["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 1, "no duplicate keys: {servers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_preserves_unrelated_keys() {
        let dir = install_tmp_dir();
        let cfg = dir.join("mcp_config.json");
        std::fs::write(
            &cfg,
            r#"{"theme":"dark","mcpServers":{"treemap":{"command":"treemap","args":["serve"]}}}"#,
        )
        .unwrap();

        let out = run_install("antigravity", Some(&cfg)).unwrap();
        assert!(
            out.contains("treemap"),
            "sibling server must be listed: {out}"
        );

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed["theme"],
            serde_json::json!("dark"),
            "unrelated key must survive"
        );
        assert_eq!(
            parsed["mcpServers"]["treemap"]["command"],
            serde_json::json!("treemap"),
            "sibling server must survive"
        );
        assert!(parsed["mcpServers"]["castor"]["command"].is_string());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_updates_existing_entry_in_place() {
        let dir = install_tmp_dir();
        let cfg = dir.join("mcp_config.json");
        std::fs::write(
            &cfg,
            r#"{"mcpServers":{"castor":{"command":"stale","args":["old"]}}}"#,
        )
        .unwrap();

        run_install("claude", Some(&cfg)).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let spec = &parsed["mcpServers"]["castor"];
        assert_ne!(
            spec["command"],
            serde_json::json!("stale"),
            "stale entry must be replaced"
        );
        assert_eq!(spec["args"], serde_json::json!(["mcp"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_rejects_invalid_client() {
        let dir = install_tmp_dir();
        let cfg = dir.join("mcp_config.json");
        let err = run_install("gemini", Some(&cfg)).unwrap_err();
        assert!(err.contains("unknown client"), "{err}");
        assert!(!cfg.exists(), "no file may be written on a bad client");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_all_writes_both_targets() {
        let dir = install_tmp_dir();
        let cfg = dir.join("mcp_config.json");
        let out = run_install("all", Some(&cfg)).unwrap();
        // With a single override path both targets write the same file;
        // the summary must mention it twice (one line per target).
        assert_eq!(out.lines().count(), 2, "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
