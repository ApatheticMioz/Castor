//! Configuration loading and management.
//!
//! Precedence (highest first): environment variables (`CASTOR_*`) >
//! `<state_dir>/config.json` > built-in defaults.
//!
//! The state dir is `$CASTOR_STATE_DIR` or `~/.castor`. There are no
//! aliases, no migration, and no profiles: one key per setting.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where a field's effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    File,
    Default,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::File => "file",
            Source::Default => "default",
        }
    }
}

/// Ports used by the castor services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ports {
    pub engine: u16,
    pub status: u16,
    pub proxy: u16,
}

impl Default for Ports {
    fn default() -> Self {
        Self {
            engine: 18020,
            status: 18021,
            proxy: 18022,
        }
    }
}

/// Effective castor configuration.
#[derive(Debug, Clone, Serialize)]
pub struct Config {
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// Secret: never print the value.
    pub api_key: Option<String>,
    pub engine_type: Option<String>,
    pub launch_command: Option<String>,
    pub stop_command: Option<String>,
    pub max_context: Option<u32>,
    pub ports: Ports,
    pub max_concurrent_tasks: u32,
    pub tool_prefix: String,
    pub searxng_url: Option<String>,
    /// Secret: never print the value.
    pub brave_api_key: Option<String>,
    pub openalex_email: Option<String>,
    /// Secret: never print the value.
    pub openalex_api_key: Option<String>,
    pub boot_timeout_secs: u64,
    /// Consecutive non-mutating bash probes before a probe-budget advisory
    /// is injected.
    pub probe_budget: usize,
    pub state_dir: PathBuf,
}

/// Effective config plus per-field provenance.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub sources: Sources,
}

/// Per-field provenance for the effective config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sources {
    pub model: Source,
    pub base_url: Source,
    pub api_key: Source,
    pub engine_type: Source,
    pub launch_command: Source,
    pub stop_command: Source,
    pub max_context: Source,
    pub port_engine: Source,
    pub port_status: Source,
    pub port_proxy: Source,
    pub max_concurrent_tasks: Source,
    pub tool_prefix: Source,
    pub searxng_url: Source,
    pub brave_api_key: Source,
    pub openalex_email: Source,
    pub openalex_api_key: Source,
    pub boot_timeout_secs: Source,
    pub probe_budget: Source,
    pub state_dir: Source,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON in config file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid value for {key} (got {value:?}): {reason}")]
    InvalidValue {
        key: String,
        value: String,
        reason: String,
    },
}

/// On-disk config file. Every field optional so partial files merge over
/// defaults; unknown keys are rejected (no aliases, no migration).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    engine_type: Option<String>,
    launch_command: Option<String>,
    stop_command: Option<String>,
    max_context: Option<u32>,
    ports: Option<PartialPorts>,
    max_concurrent_tasks: Option<u32>,
    tool_prefix: Option<String>,
    searxng_url: Option<String>,
    brave_api_key: Option<String>,
    openalex_email: Option<String>,
    openalex_api_key: Option<String>,
    boot_timeout_secs: Option<u64>,
    probe_budget: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialPorts {
    engine: Option<u16>,
    status: Option<u16>,
    proxy: Option<u16>,
}

/// Load the effective configuration from the real environment.
pub fn load() -> Result<LoadedConfig, ConfigError> {
    load_with(|key| std::env::var(key).ok())
}

/// Load with an injectable env getter (test seam).
pub fn load_with(get_env: impl Fn(&str) -> Option<String>) -> Result<LoadedConfig, ConfigError> {
    let (state_dir, state_dir_src) = match get_env("CASTOR_STATE_DIR") {
        Some(v) if !v.is_empty() => (PathBuf::from(v), Source::Env),
        _ => (default_state_dir(), Source::Default),
    };

    let file_path = state_dir.join("config.json");
    let file = if file_path.is_file() {
        let raw = std::fs::read_to_string(&file_path).map_err(|e| ConfigError::Read {
            path: file_path.clone(),
            source: e,
        })?;
        let clean = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        Some(
            serde_json::from_str::<FileConfig>(clean).map_err(|e| ConfigError::Parse {
                path: file_path.clone(),
                source: e,
            })?,
        )
    } else {
        None
    };
    let file = file.as_ref();

    let env_str = |key: &str| -> Option<String> { get_env(key).filter(|v| !v.is_empty()) };
    let env_u32 = |key: &str| -> Result<Option<u32>, ConfigError> {
        match get_env(key) {
            Some(v) if !v.is_empty() => {
                v.parse::<u32>()
                    .map(Some)
                    .map_err(|e| ConfigError::InvalidValue {
                        key: key.to_string(),
                        value: v,
                        reason: e.to_string(),
                    })
            }
            _ => Ok(None),
        }
    };
    let env_u16 = |key: &str| -> Result<Option<u16>, ConfigError> {
        match get_env(key) {
            Some(v) if !v.is_empty() => {
                v.parse::<u16>()
                    .map(Some)
                    .map_err(|e| ConfigError::InvalidValue {
                        key: key.to_string(),
                        value: v,
                        reason: e.to_string(),
                    })
            }
            _ => Ok(None),
        }
    };
    let env_u64 = |key: &str| -> Result<Option<u64>, ConfigError> {
        match get_env(key) {
            Some(v) if !v.is_empty() => {
                v.parse::<u64>()
                    .map(Some)
                    .map_err(|e| ConfigError::InvalidValue {
                        key: key.to_string(),
                        value: v,
                        reason: e.to_string(),
                    })
            }
            _ => Ok(None),
        }
    };

    let env_max_context = env_u32("CASTOR_MAX_CONTEXT")?;
    let env_max_concurrent = env_u32("CASTOR_MAX_CONCURRENT_TASKS")?;
    let env_boot_timeout = env_u64("CASTOR_BOOT_TIMEOUT_SECS")?;
    let env_port_engine = env_u16("CASTOR_PORT_ENGINE")?;
    let env_port_status = env_u16("CASTOR_PORT_STATUS")?;
    let env_port_proxy = env_u16("CASTOR_PORT_PROXY")?;

    let file_ports = file.and_then(|f| f.ports.as_ref());

    let (model, src_model) = pick_str(env_str("CASTOR_MODEL"), file.and_then(|f| f.model.clone()));
    let (base_url, src_base_url) = pick_str(
        env_str("CASTOR_BASE_URL"),
        file.and_then(|f| f.base_url.clone()),
    );
    let (api_key, src_api_key) = pick_str(
        env_str("CASTOR_API_KEY"),
        file.and_then(|f| f.api_key.clone()),
    );
    let (engine_type, src_engine_type) = pick_str(
        env_str("CASTOR_ENGINE_TYPE"),
        file.and_then(|f| f.engine_type.clone()),
    );
    let (launch_command, src_launch_command) = pick_str(
        env_str("CASTOR_LAUNCH_COMMAND"),
        file.and_then(|f| f.launch_command.clone()),
    );
    let (stop_command, src_stop_command) = pick_str(
        env_str("CASTOR_STOP_COMMAND"),
        file.and_then(|f| f.stop_command.clone()),
    );
    let (searxng_url, src_searxng_url) = pick_str(
        env_str("CASTOR_SEARXNG_URL"),
        file.and_then(|f| f.searxng_url.clone()),
    );
    let (brave_api_key, src_brave_api_key) = pick_str(
        env_str("CASTOR_BRAVE_API_KEY"),
        file.and_then(|f| f.brave_api_key.clone()),
    );
    let (openalex_email, src_openalex_email) = pick_str(
        env_str("CASTOR_OPENALEX_EMAIL"),
        file.and_then(|f| f.openalex_email.clone()),
    );
    let (openalex_api_key, src_openalex_api_key) = pick_str(
        env_str("CASTOR_OPENALEX_API_KEY"),
        file.and_then(|f| f.openalex_api_key.clone()),
    );

    let (max_context, src_max_context) =
        pick_opt_u32(env_max_context, file.and_then(|f| f.max_context));
    let (max_concurrent_tasks, src_max_concurrent_tasks) = pick_u32(
        env_max_concurrent,
        file.and_then(|f| f.max_concurrent_tasks),
        1,
    );
    let (tool_prefix, src_tool_prefix) = pick_str_default(
        env_str("CASTOR_TOOL_PREFIX"),
        file.and_then(|f| f.tool_prefix.clone()),
        "castor".to_string(),
    );
    let (boot_timeout_secs, src_boot_timeout_secs) = pick_u64(
        env_boot_timeout,
        file.and_then(|f| f.boot_timeout_secs),
        180,
    );

    // Probe budget: env > file > default (4).
    let env_probe_budget: Option<usize> = env_u32("CASTOR_PROBE_BUDGET")?.map(|v| v as usize);
    let (probe_budget, src_probe_budget) =
        pick_usize(env_probe_budget, file.and_then(|f| f.probe_budget), 4);

    let (ports, (src_port_engine, src_port_status, src_port_proxy)) = {
        let (engine, src_port_engine) =
            pick_u16(env_port_engine, file_ports.and_then(|p| p.engine), 18020);
        let (status, src_port_status) =
            pick_u16(env_port_status, file_ports.and_then(|p| p.status), 18021);
        let (proxy, src_port_proxy) =
            pick_u16(env_port_proxy, file_ports.and_then(|p| p.proxy), 18022);
        (
            Ports {
                engine,
                status,
                proxy,
            },
            (src_port_engine, src_port_status, src_port_proxy),
        )
    };

    Ok(LoadedConfig {
        config: Config {
            model,
            base_url,
            api_key,
            engine_type,
            launch_command,
            stop_command,
            max_context,
            ports,
            max_concurrent_tasks,
            tool_prefix,
            searxng_url,
            brave_api_key,
            openalex_email,
            openalex_api_key,
            boot_timeout_secs,
            probe_budget,
            state_dir,
        },
        sources: Sources {
            model: src_model,
            base_url: src_base_url,
            api_key: src_api_key,
            engine_type: src_engine_type,
            launch_command: src_launch_command,
            stop_command: src_stop_command,
            max_context: src_max_context,
            port_engine: src_port_engine,
            port_status: src_port_status,
            port_proxy: src_port_proxy,
            max_concurrent_tasks: src_max_concurrent_tasks,
            tool_prefix: src_tool_prefix,
            searxng_url: src_searxng_url,
            brave_api_key: src_brave_api_key,
            openalex_email: src_openalex_email,
            openalex_api_key: src_openalex_api_key,
            boot_timeout_secs: src_boot_timeout_secs,
            probe_budget: src_probe_budget,
            state_dir: state_dir_src,
        },
    })
}

/// Render the effective config with per-field source annotations.
/// Secrets are masked as `***`.
pub fn format_loaded(loaded: &LoadedConfig) -> String {
    let c = &loaded.config;
    let s = &loaded.sources;
    let mut out = String::new();
    out.push_str("castor effective config\n");
    row(&mut out, "model", &display(c.model.as_deref()), s.model);
    row(
        &mut out,
        "base_url",
        &display(c.base_url.as_deref()),
        s.base_url,
    );
    row(
        &mut out,
        "api_key",
        &display_secret(c.api_key.as_deref()),
        s.api_key,
    );
    row(
        &mut out,
        "engine_type",
        &display(c.engine_type.as_deref()),
        s.engine_type,
    );
    row(
        &mut out,
        "launch_command",
        &display(c.launch_command.as_deref()),
        s.launch_command,
    );
    row(
        &mut out,
        "stop_command",
        &display(c.stop_command.as_deref()),
        s.stop_command,
    );
    row(
        &mut out,
        "max_context",
        &display(c.max_context.map(|v| v.to_string()).as_deref()),
        s.max_context,
    );
    row(
        &mut out,
        "ports.engine",
        &c.ports.engine.to_string(),
        s.port_engine,
    );
    row(
        &mut out,
        "ports.status",
        &c.ports.status.to_string(),
        s.port_status,
    );
    row(
        &mut out,
        "ports.proxy",
        &c.ports.proxy.to_string(),
        s.port_proxy,
    );
    row(
        &mut out,
        "max_concurrent_tasks",
        &c.max_concurrent_tasks.to_string(),
        s.max_concurrent_tasks,
    );
    row(&mut out, "tool_prefix", &c.tool_prefix, s.tool_prefix);
    row(
        &mut out,
        "searxng_url",
        &display(c.searxng_url.as_deref()),
        s.searxng_url,
    );
    row(
        &mut out,
        "brave_api_key",
        &display_secret(c.brave_api_key.as_deref()),
        s.brave_api_key,
    );
    row(
        &mut out,
        "openalex_email",
        &display(c.openalex_email.as_deref()),
        s.openalex_email,
    );
    row(
        &mut out,
        "openalex_api_key",
        &display_secret(c.openalex_api_key.as_deref()),
        s.openalex_api_key,
    );
    row(
        &mut out,
        "boot_timeout_secs",
        &c.boot_timeout_secs.to_string(),
        s.boot_timeout_secs,
    );
    row(
        &mut out,
        "probe_budget",
        &c.probe_budget.to_string(),
        s.probe_budget,
    );
    row(
        &mut out,
        "state_dir",
        &c.state_dir.display().to_string(),
        s.state_dir,
    );
    out
}

/// Load and render the effective config.
pub fn format_effective() -> Result<String, ConfigError> {
    Ok(format_loaded(&load()?))
}

/// Load and print the effective config (used by `castor config`).
pub fn print_effective() -> Result<(), ConfigError> {
    print!("{}", format_effective()?);
    Ok(())
}

fn row(out: &mut String, name: &str, value: &str, source: Source) {
    out.push_str(&format!(
        "{:<20} = {:<40} [{}]\n",
        name,
        value,
        source.as_str()
    ));
}

fn display(v: Option<&str>) -> String {
    v.map(str::to_string)
        .unwrap_or_else(|| "<unset>".to_string())
}

fn display_secret(v: Option<&str>) -> String {
    match v {
        Some(_) => "***".to_string(),
        None => "<unset>".to_string(),
    }
}

fn pick_str(env: Option<String>, file: Option<String>) -> (Option<String>, Source) {
    match (env.as_ref(), file.as_ref()) {
        (Some(_), _) => (env, Source::Env),
        (None, Some(_)) => (file, Source::File),
        (None, None) => (None, Source::Default),
    }
}

fn pick_str_default(
    env: Option<String>,
    file: Option<String>,
    default: String,
) -> (String, Source) {
    match (env.as_ref(), file.as_ref()) {
        (Some(_), _) => (env.unwrap(), Source::Env),
        (None, Some(_)) => (file.unwrap(), Source::File),
        (None, None) => (default, Source::Default),
    }
}

fn pick_opt_u32(env: Option<u32>, file: Option<u32>) -> (Option<u32>, Source) {
    match (env, file) {
        (Some(_), _) => (env, Source::Env),
        (None, Some(_)) => (file, Source::File),
        (None, None) => (None, Source::Default),
    }
}

fn pick_u64(env: Option<u64>, file: Option<u64>, default: u64) -> (u64, Source) {
    match (env, file) {
        (Some(_), _) => (env.unwrap(), Source::Env),
        (None, Some(_)) => (file.unwrap(), Source::File),
        (None, None) => (default, Source::Default),
    }
}

fn pick_u32(env: Option<u32>, file: Option<u32>, default: u32) -> (u32, Source) {
    match (env, file) {
        (Some(_), _) => (env.unwrap(), Source::Env),
        (None, Some(_)) => (file.unwrap(), Source::File),
        (None, None) => (default, Source::Default),
    }
}

fn pick_u16(env: Option<u16>, file: Option<u16>, default: u16) -> (u16, Source) {
    match (env, file) {
        (Some(_), _) => (env.unwrap(), Source::Env),
        (None, Some(_)) => (file.unwrap(), Source::File),
        (None, None) => (default, Source::Default),
    }
}

fn pick_usize(env: Option<usize>, file: Option<usize>, default: usize) -> (usize, Source) {
    match (env, file) {
        (Some(_), _) => (env.unwrap(), Source::Env),
        (None, Some(_)) => (file.unwrap(), Source::File),
        (None, None) => (default, Source::Default),
    }
}

fn default_state_dir() -> PathBuf {
    home_dir()
        .map(|h| h.join(".castor"))
        .unwrap_or_else(|| PathBuf::from(".castor"))
}

fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp_dir() -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("castor-cfg-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Load with the given env pairs; the state dir is pinned to a fresh
    /// temp dir (unless the test overrides CASTOR_STATE_DIR itself).
    fn load_case(env: &[(&str, &str)], file_json: Option<&str>) -> LoadedConfig {
        let dir = tmp_dir();
        if let Some(json) = file_json {
            std::fs::write(dir.join("config.json"), json).unwrap();
        }
        let mut env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if !env.iter().any(|(k, _)| k == "CASTOR_STATE_DIR") {
            env.push(("CASTOR_STATE_DIR".to_string(), dir.display().to_string()));
        }
        load_with(move |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.clone()))
            .unwrap()
    }

    fn load_with_env(env: &[(&str, &str)]) -> Result<LoadedConfig, ConfigError> {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        load_with(move |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.clone()))
    }

    /// Check env > file > default for one optional string field.
    fn check_str_field(
        env_key: &str,
        json_key: &str,
        env_val: &str,
        file_val: &str,
        get: impl Fn(&LoadedConfig) -> (Option<String>, Source),
    ) {
        let json = format!("{{\"{json_key}\":\"{file_val}\"}}");
        let l = load_case(&[(env_key, env_val)], Some(&json));
        assert_eq!(
            get(&l),
            (Some(env_val.to_string()), Source::Env),
            "{env_key}: env must beat file"
        );
        let l = load_case(&[], Some(&json));
        assert_eq!(
            get(&l),
            (Some(file_val.to_string()), Source::File),
            "{env_key}: file must beat default"
        );
        let l = load_case(&[], None);
        assert_eq!(
            get(&l),
            (None, Source::Default),
            "{env_key}: default when unset"
        );
    }

    #[test]
    fn round_trip_full_file() {
        let json = r#"{
            "model": "m1",
            "base_url": "https://api.example.com/v1",
            "api_key": "sk-file",
            "engine_type": "qwen",
            "launch_command": "qwen -s",
            "stop_command": "qwen -k",
            "max_context": 32768,
            "ports": { "engine": 1, "status": 2, "proxy": 3 },
            "max_concurrent_tasks": 7,
            "tool_prefix": "cast",
            "searxng_url": "https://searxng.example",
            "brave_api_key": "bkey",
            "openalex_email": "user@example.com",
            "openalex_api_key": "oakey",
            "boot_timeout_secs": 240
        }"#;
        let l = load_case(&[], Some(json));
        assert_eq!(l.config.model.as_deref(), Some("m1"));
        assert_eq!(
            l.config.base_url.as_deref(),
            Some("https://api.example.com/v1")
        );
        assert_eq!(l.config.api_key.as_deref(), Some("sk-file"));
        assert_eq!(l.config.engine_type.as_deref(), Some("qwen"));
        assert_eq!(l.config.launch_command.as_deref(), Some("qwen -s"));
        assert_eq!(l.config.stop_command.as_deref(), Some("qwen -k"));
        assert_eq!(l.config.max_context, Some(32768));
        assert_eq!(
            l.config.ports,
            Ports {
                engine: 1,
                status: 2,
                proxy: 3
            }
        );
        assert_eq!(l.config.max_concurrent_tasks, 7);
        assert_eq!(l.config.tool_prefix, "cast");
        assert_eq!(
            l.config.searxng_url.as_deref(),
            Some("https://searxng.example")
        );
        assert_eq!(l.config.brave_api_key.as_deref(), Some("bkey"));
        assert_eq!(l.config.openalex_email.as_deref(), Some("user@example.com"));
        assert_eq!(l.config.openalex_api_key.as_deref(), Some("oakey"));
        assert_eq!(l.config.boot_timeout_secs, 240);
        // every file-provided field is annotated as File
        assert_eq!(l.sources.model, Source::File);
        assert_eq!(l.sources.base_url, Source::File);
        assert_eq!(l.sources.api_key, Source::File);
        assert_eq!(l.sources.engine_type, Source::File);
        assert_eq!(l.sources.launch_command, Source::File);
        assert_eq!(l.sources.stop_command, Source::File);
        assert_eq!(l.sources.max_context, Source::File);
        assert_eq!(l.sources.port_engine, Source::File);
        assert_eq!(l.sources.port_status, Source::File);
        assert_eq!(l.sources.port_proxy, Source::File);
        assert_eq!(l.sources.max_concurrent_tasks, Source::File);
        assert_eq!(l.sources.tool_prefix, Source::File);
        assert_eq!(l.sources.searxng_url, Source::File);
        assert_eq!(l.sources.brave_api_key, Source::File);
        assert_eq!(l.sources.openalex_email, Source::File);
        assert_eq!(l.sources.openalex_api_key, Source::File);
        assert_eq!(l.sources.boot_timeout_secs, Source::File);
        // state dir comes from the env pin in load_case
        assert_eq!(l.sources.state_dir, Source::Env);
    }

    #[test]
    fn precedence_string_fields() {
        check_str_field("CASTOR_MODEL", "model", "env-model", "file-model", |l| {
            (l.config.model.clone(), l.sources.model)
        });
        check_str_field("CASTOR_BASE_URL", "base_url", "env-bu", "file-bu", |l| {
            (l.config.base_url.clone(), l.sources.base_url)
        });
        check_str_field("CASTOR_API_KEY", "api_key", "env-key", "file-key", |l| {
            (l.config.api_key.clone(), l.sources.api_key)
        });
        check_str_field(
            "CASTOR_ENGINE_TYPE",
            "engine_type",
            "env-et",
            "file-et",
            |l| (l.config.engine_type.clone(), l.sources.engine_type),
        );
        check_str_field(
            "CASTOR_LAUNCH_COMMAND",
            "launch_command",
            "env-lc",
            "file-lc",
            |l| (l.config.launch_command.clone(), l.sources.launch_command),
        );
        check_str_field(
            "CASTOR_STOP_COMMAND",
            "stop_command",
            "env-sc",
            "file-sc",
            |l| (l.config.stop_command.clone(), l.sources.stop_command),
        );
        check_str_field(
            "CASTOR_SEARXNG_URL",
            "searxng_url",
            "env-sx",
            "file-sx",
            |l| (l.config.searxng_url.clone(), l.sources.searxng_url),
        );
        check_str_field(
            "CASTOR_BRAVE_API_KEY",
            "brave_api_key",
            "env-bk",
            "file-bk",
            |l| (l.config.brave_api_key.clone(), l.sources.brave_api_key),
        );
        check_str_field(
            "CASTOR_OPENALEX_EMAIL",
            "openalex_email",
            "env-oe",
            "file-oe",
            |l| (l.config.openalex_email.clone(), l.sources.openalex_email),
        );
        check_str_field(
            "CASTOR_OPENALEX_API_KEY",
            "openalex_api_key",
            "env-oa",
            "file-oa",
            |l| {
                (
                    l.config.openalex_api_key.clone(),
                    l.sources.openalex_api_key,
                )
            },
        );
    }

    #[test]
    fn precedence_max_context() {
        let l = load_case(
            &[("CASTOR_MAX_CONTEXT", "4096")],
            Some(r#"{"max_context":8192}"#),
        );
        assert_eq!(
            (l.config.max_context, l.sources.max_context),
            (Some(4096), Source::Env)
        );
        let l = load_case(&[], Some(r#"{"max_context":8192}"#));
        assert_eq!(
            (l.config.max_context, l.sources.max_context),
            (Some(8192), Source::File)
        );
        let l = load_case(&[], None);
        assert_eq!(
            (l.config.max_context, l.sources.max_context),
            (None, Source::Default)
        );
    }

    #[test]
    fn precedence_max_concurrent_tasks() {
        let l = load_case(
            &[("CASTOR_MAX_CONCURRENT_TASKS", "5")],
            Some(r#"{"max_concurrent_tasks":3}"#),
        );
        assert_eq!(
            (
                l.config.max_concurrent_tasks,
                l.sources.max_concurrent_tasks
            ),
            (5, Source::Env)
        );
        let l = load_case(&[], Some(r#"{"max_concurrent_tasks":3}"#));
        assert_eq!(
            (
                l.config.max_concurrent_tasks,
                l.sources.max_concurrent_tasks
            ),
            (3, Source::File)
        );
        let l = load_case(&[], None);
        assert_eq!(
            (
                l.config.max_concurrent_tasks,
                l.sources.max_concurrent_tasks
            ),
            (1, Source::Default)
        );
    }

    #[test]
    fn precedence_tool_prefix() {
        let l = load_case(
            &[("CASTOR_TOOL_PREFIX", "envp")],
            Some(r#"{"tool_prefix":"filep"}"#),
        );
        assert_eq!(
            (l.config.tool_prefix.as_str(), l.sources.tool_prefix),
            ("envp", Source::Env)
        );
        let l = load_case(&[], Some(r#"{"tool_prefix":"filep"}"#));
        assert_eq!(
            (l.config.tool_prefix.as_str(), l.sources.tool_prefix),
            ("filep", Source::File)
        );
        let l = load_case(&[], None);
        assert_eq!(
            (l.config.tool_prefix.as_str(), l.sources.tool_prefix),
            ("castor", Source::Default)
        );
    }

    #[test]
    fn precedence_ports() {
        let l = load_case(
            &[
                ("CASTOR_PORT_ENGINE", "1111"),
                ("CASTOR_PORT_STATUS", "2222"),
            ],
            Some(r#"{"ports":{"engine":100,"status":200,"proxy":300}}"#),
        );
        assert_eq!(
            (l.config.ports.engine, l.sources.port_engine),
            (1111, Source::Env)
        );
        assert_eq!(
            (l.config.ports.status, l.sources.port_status),
            (2222, Source::Env)
        );
        assert_eq!(
            (l.config.ports.proxy, l.sources.port_proxy),
            (300, Source::File)
        );
        let l = load_case(&[], None);
        assert_eq!(l.config.ports, Ports::default());
        assert_eq!(l.sources.port_engine, Source::Default);
        assert_eq!(l.sources.port_status, Source::Default);
        assert_eq!(l.sources.port_proxy, Source::Default);
    }

    #[test]
    fn precedence_state_dir() {
        let l = load_case(&[("CASTOR_STATE_DIR", "/tmp/castor-x")], None);
        assert_eq!(l.config.state_dir, PathBuf::from("/tmp/castor-x"));
        assert_eq!(l.sources.state_dir, Source::Env);
    }

    #[test]
    fn partial_file_merges_over_defaults() {
        let l = load_case(&[], Some(r#"{"model":"m","ports":{"status":1234}}"#));
        assert_eq!(
            (l.config.model.as_deref(), l.sources.model),
            (Some("m"), Source::File)
        );
        assert_eq!(
            (l.config.ports.status, l.sources.port_status),
            (1234, Source::File)
        );
        // unspecified port falls back to the built-in default
        assert_eq!(
            (l.config.ports.engine, l.sources.port_engine),
            (18020, Source::Default)
        );
        assert_eq!(
            (l.config.ports.proxy, l.sources.port_proxy),
            (18022, Source::Default)
        );
        // other defaults
        assert_eq!(
            (
                l.config.max_concurrent_tasks,
                l.sources.max_concurrent_tasks
            ),
            (1, Source::Default)
        );
        assert_eq!(
            (l.config.tool_prefix.as_str(), l.sources.tool_prefix),
            ("castor", Source::Default)
        );
        assert_eq!(
            (l.config.max_context, l.sources.max_context),
            (None, Source::Default)
        );
        assert_eq!(l.config.base_url, None);
    }

    #[test]
    fn invalid_json_fails_fast() {
        let dir = tmp_dir();
        std::fs::write(dir.join("config.json"), "{ not json !").unwrap();
        let err = load_with_env(&[("CASTOR_STATE_DIR", &dir.display().to_string())]).unwrap_err();
        match &err {
            ConfigError::Parse { path, .. } => {
                assert_eq!(*path, dir.join("config.json"));
                let msg = err.to_string();
                assert!(msg.contains("invalid JSON"), "{msg}");
                assert!(msg.contains("config.json"), "{msg}");
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_key_in_file_fails() {
        let dir = tmp_dir();
        std::fs::write(
            dir.join("config.json"),
            r#"{"model":"m","model_alias":"x"}"#,
        )
        .unwrap();
        let err = load_with_env(&[("CASTOR_STATE_DIR", &dir.display().to_string())]).unwrap_err();
        match &err {
            ConfigError::Parse { .. } => {}
            other => panic!("expected Parse error for unknown key, got {other:?}"),
        }
    }

    #[test]
    fn invalid_env_number_fails() {
        let dir = tmp_dir();
        let err = load_with_env(&[
            ("CASTOR_STATE_DIR", &dir.display().to_string()),
            ("CASTOR_MAX_CONTEXT", "not-a-number"),
        ])
        .unwrap_err();
        match err {
            ConfigError::InvalidValue { key, value, .. } => {
                assert_eq!(key, "CASTOR_MAX_CONTEXT");
                assert_eq!(value, "not-a-number");
            }
            other => panic!("expected InvalidValue, got {other:?}"),
        }
    }

    #[test]
    fn precedence_probe_budget() {
        let l = load_case(
            &[("CASTOR_PROBE_BUDGET", "8")],
            Some(r#"{"probe_budget":6}"#),
        );
        assert_eq!(
            (l.config.probe_budget, l.sources.probe_budget),
            (8, Source::Env)
        );
        let l = load_case(&[], Some(r#"{"probe_budget":6}"#));
        assert_eq!(
            (l.config.probe_budget, l.sources.probe_budget),
            (6, Source::File)
        );
        let l = load_case(&[], None);
        assert_eq!(
            (l.config.probe_budget, l.sources.probe_budget),
            (4, Source::Default)
        );
    }

    #[test]
    fn port_override_via_env() {
        let l = load_case(
            &[("CASTOR_PORT_PROXY", "9999")],
            Some(r#"{"ports":{"proxy":8888}}"#),
        );
        assert_eq!(
            (l.config.ports.proxy, l.sources.port_proxy),
            (9999, Source::Env)
        );
        let l = load_case(&[], Some(r#"{"ports":{"proxy":8888}}"#));
        assert_eq!(
            (l.config.ports.proxy, l.sources.port_proxy),
            (8888, Source::File)
        );
    }

    #[test]
    fn api_key_never_printed() {
        let l = load_case(
            &[("CASTOR_API_KEY", "sk-env-secret")],
            Some(r#"{"api_key":"sk-file-secret","brave_api_key":"bkey-file"}"#),
        );
        let text = format_loaded(&l);
        assert!(text.contains("***"), "{text}");
        assert!(!text.contains("sk-env-secret"), "{text}");
        assert!(!text.contains("sk-file-secret"), "{text}");
        assert!(!text.contains("bkey-file"), "{text}");
    }

    #[test]
    fn default_state_dir_under_home() {
        let d = default_state_dir();
        assert!(d.to_string_lossy().ends_with(".castor"), "got {d:?}");
    }

    #[test]
    fn file_config_with_utf8_bom() {
        let bom_json = format!("\u{feff}{}", r#"{"model":"bom-test"}"#);
        let l = load_case(&[], Some(&bom_json));
        assert_eq!(l.config.model.as_deref(), Some("bom-test"));
        assert_eq!(l.sources.model, Source::File);
    }
}
