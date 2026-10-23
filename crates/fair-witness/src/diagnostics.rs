use std::env::{self, VarError};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use fair_witness_core::project;
use tracing::{Level, Subscriber};
use tracing_subscriber::filter::{EnvFilter, FilterExt, filter_fn};
use tracing_subscriber::fmt::{self, MakeWriter};
use tracing_subscriber::prelude::*;

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
    subscriber(filter, Mutex::new(file))
        .try_init()
        .map_err(|e| anyhow!("could not start the diagnostics log: {e}"))
}

// The screen owns the terminal, so diagnostics never go to stdout or stderr.
fn subscriber<W>(filter: EnvFilter, writer: W) -> impl Subscriber + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    // A filter of its own, because a more specific directive in `filter`, such as
    // `notify::inotify=trace`, would win over any directive added there.
    let quiet_watcher =
        filter_fn(|metadata| *metadata.level() <= Level::WARN || !from_watcher(metadata.target()));
    tracing_subscriber::registry().with(
        fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_filter(filter.and(quiet_watcher)),
    )
}

fn from_watcher(target: &str) -> bool {
    WATCHER_CRATES.iter().any(|name| {
        target
            .strip_prefix(name)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

fn filter(level: Result<String, VarError>) -> Result<EnvFilter> {
    let level = match level {
        Ok(level) if !level.trim().is_empty() => level,
        Ok(_) | Err(VarError::NotPresent) => DEFAULT_LEVEL.to_string(),
        Err(e) => return Err(e).with_context(|| format!("{LEVEL_VARIABLE} is not valid")),
    };
    // The error repeats its message as its source, so only its message is kept.
    EnvFilter::try_new(&level).map_err(|e| {
        anyhow!(
            "{LEVEL_VARIABLE}={level} is not a log level ({e}); use one such as `info` or `debug`"
        )
    })
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::Arc;

    use super::*;

    #[derive(Clone, Default)]
    struct Written(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Written {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn keeps_the_watcher_below_warn_out_whatever_the_level() {
        let written = Written::default();
        let level = "trace,notify=trace,notify::inotify=trace,notify_debouncer_full=trace";
        let filter = filter(Ok(level.to_string())).unwrap();
        let make_writer = {
            let written = written.clone();
            move || written.clone()
        };

        tracing::subscriber::with_default(subscriber(filter, make_writer), || {
            tracing::trace!(target: "notify::inotify", "inotify event");
            tracing::debug!(target: "notify_debouncer_full", "raw event");
            tracing::warn!(target: "notify", "watch failed");
            tracing::trace!(target: "notify_rust", "notification shown");
            tracing::trace!(target: "fair_witness_core::session", "event logged");
        });

        let written = String::from_utf8(written.0.lock().unwrap().clone()).unwrap();
        let messages: Vec<&str> = written
            .lines()
            .map(|line| line.split_once(": ").unwrap().1)
            .collect();
        assert_eq!(
            messages,
            ["watch failed", "notification shown", "event logged"]
        );
    }

    #[test]
    fn reads_the_level_and_says_how_to_fix_a_wrong_one() {
        let level = |value: &str| filter(Ok(value.to_string())).map(|f| f.to_string());

        assert_eq!(
            filter(Err(VarError::NotPresent)).unwrap().to_string(),
            "info"
        );
        assert_eq!(level(" ").unwrap(), "info");
        assert_eq!(
            level("info=loud").unwrap_err().to_string(),
            "FAIR_WITNESS_LOG=info=loud is not a log level (error parsing level filter: expected \
             one of \"off\", \"error\", \"warn\", \"info\", \"debug\", \"trace\", or a number \
             0-5); use one such as `info` or `debug`"
        );
    }
}
