//! Telemetry event emission: tracing-subscriber initialization and the
//! JSONL file sink that backs the single event stream.
//!
//! The tracing-JSONL ledger (`<state>/telemetry/events.jsonl`) is THE ledger.
//! [`init_tracing`] installs the global default subscriber (at most once per
//! process): one JSON object per line appended to the ledger, plus a
//! `warn`-and-above mirror to stderr for the MCP-mode operator surface.

use std::fs;
use std::path::{Path, PathBuf};

use tracing_subscriber::prelude::*;

/// Initialize the single tracing-JSONL event stream.
///
/// Writes one JSON object per line to `<state_dir>/telemetry/events.jsonl`
/// (append mode, created on demand) and mirrors `warn`-and-above events to
/// stderr (the MCP-mode operator surface). The level filter honors the
/// `RUST_LOG` environment variable (defaulting to `info`).
///
/// This installs the *global* default subscriber, so it must be called at
/// most once per process.
pub fn init_tracing(state_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let telemetry_dir = state_dir.join("telemetry");
    fs::create_dir_all(&telemetry_dir)?;
    let events_path = telemetry_dir.join("events.jsonl");

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(FileSink { path: events_path })
        .with_filter(env_filter);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_filter(tracing_subscriber::EnvFilter::new("warn"));

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init()?;

    Ok(())
}

/// A `MakeWriter` that appends to a fixed file path (one open per write).
///
/// Telemetry must never break the inference path: if the file (or its
/// parent directory) cannot be opened, the writer degrades to a no-op
/// sink instead of panicking.
struct FileSink {
    path: PathBuf,
}

/// A `Write` target that silently discards every byte.
struct NoopWriter;

impl std::io::Write for NoopWriter {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Ok(_buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for FileSink {
    type Writer = Box<dyn std::io::Write + Send + Sync>;

    fn make_writer(&'writer self) -> Self::Writer {
        use std::fs::OpenOptions;
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => Box::new(f),
            Err(_) => Box::new(NoopWriter),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tests::tmp_state;
    use serde_json::Value;

    #[test]
    fn init_tracing_writes_json_line_to_file_sink() {
        let state = tmp_state();
        init_tracing(&state).expect("init_tracing");
        tracing::info!(target: "castor.telemetry", hello = "world", "telemetry line");

        let events = state.join("telemetry/events.jsonl");
        let raw = fs::read_to_string(&events).expect("events.jsonl exists");
        let line = raw.lines().find(|l| !l.trim().is_empty()).expect("a line");
        let v: Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(v.get("level").and_then(|x| x.as_str()), Some("INFO"));
        assert_eq!(
            v.get("fields")
                .and_then(|f| f.get("hello"))
                .and_then(|x| x.as_str()),
            Some("world")
        );
        assert!(v.get("timestamp").is_some());
        let _ = fs::remove_dir_all(&state);
    }
}
