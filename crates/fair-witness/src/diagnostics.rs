use std::env::{self, VarError};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use fair_witness_core::project;
use tracing_subscriber::EnvFilter;

/// Sets which diagnostics are written, with the syntax of `RUST_LOG`.
pub const LEVEL_VARIABLE: &str = "FAIR_WITNESS_LOG";
const DEFAULT_LEVEL: &str = "info";
/// The watcher sees the diagnostics file, which is in the watched folder. These crates log
/// every file event below warn, so writing that line would cause the next, without end.
const WATCHER_CRATES: [&str; 2] = ["notify", "notify_debouncer_full"];

/// Writes the diagnostics of this process to a new file at `path`, at the level
/// `FAIR_WITNESS_LOG` sets.
pub fn write_to(path: &Path) -> Result<()> {
    let filter = filter(env::var(LEVEL_VARIABLE))?;
    let file = project::create_private(path)?;
    // The screen owns the terminal, so diagnostics never go to stdout or stderr.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .try_init()
        .map_err(|e| anyhow!("could not start the diagnostics log: {e}"))
}

fn filter(level: Result<String, VarError>) -> Result<EnvFilter> {
    let level = match level {
        Ok(level) if !level.trim().is_empty() => level,
        Ok(_) | Err(VarError::NotPresent) => DEFAULT_LEVEL.to_string(),
        Err(e) => return Err(e).with_context(|| format!("{LEVEL_VARIABLE} is not valid")),
    };
    // The error repeats its message as its source, so only its message is kept.
    let mut filter = EnvFilter::try_new(&level).map_err(|e| {
        anyhow!(
            "{LEVEL_VARIABLE}={level} is not a log level ({e}); use one such as `info` or `debug`"
        )
    })?;
    for target in WATCHER_CRATES {
        filter = filter.add_directive(format!("{target}=warn").parse()?);
    }
    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_level_but_keeps_the_watcher_quiet() {
        let level = |value: &str| filter(Ok(value.to_string())).map(|f| f.to_string());

        let watcher = "notify_debouncer_full=warn,notify=warn";
        assert_eq!(
            filter(Err(VarError::NotPresent)).unwrap().to_string(),
            format!("{watcher},info")
        );
        assert_eq!(level(" ").unwrap(), format!("{watcher},info"));
        assert_eq!(
            level("trace,notify=trace,fair_witness_core=debug").unwrap(),
            "notify_debouncer_full=warn,fair_witness_core=debug,notify=warn,trace"
        );
        assert_eq!(
            level("info=loud").unwrap_err().to_string(),
            "FAIR_WITNESS_LOG=info=loud is not a log level (error parsing level filter: expected \
             one of \"off\", \"error\", \"warn\", \"info\", \"debug\", \"trace\", or a number \
             0-5); use one such as `info` or `debug`"
        );
    }
}
