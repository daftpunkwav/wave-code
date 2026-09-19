//! File logging setup: a daily rolling file under `~/.wavecode/logs`.
//!
//! Every surface logs to the file only — never stdout/stderr — so the
//! exec JSONL protocol stream and the TUI rendering stay clean. Level
//! resolution: `--debug` wins over `WAVECODE_LOG`, which wins over the
//! `warn` default; an unparsable `WAVECODE_LOG` falls back to `warn`.

use tracing_subscriber::EnvFilter;

/// Resolve the effective level directive: `--debug` > `WAVECODE_LOG` > `warn`.
fn level_directive_for(debug: bool, env: Option<&str>) -> String {
    if debug {
        return "debug".to_string();
    }
    match env.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => "warn".to_string(),
    }
}

/// Resolve from the real environment.
fn level_directive(debug: bool) -> String {
    level_directive_for(debug, std::env::var("WAVECODE_LOG").ok().as_deref())
}

/// Build the daily rolling appender under `<home>/.wavecode/logs`, or
/// `None` when the directory cannot be created or opened.
fn file_appender()
-> Option<impl for<'writer> tracing_subscriber::fmt::writer::MakeWriter<'writer> + 'static> {
    let dir = wavecode_config::home_dir()?.join(".wavecode").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("wavecode.log")
        .max_log_files(14)
        .build(&dir)
        .ok()
}

/// Install the global tracing subscriber.
///
/// Best-effort file logging: when the log directory is unavailable a
/// stderr subscriber at the same level keeps `tracing::warn!` call
/// sites observable instead of silently dropping them again.
pub fn init(debug: bool) {
    let directive = level_directive(debug);
    let filter = EnvFilter::try_new(&directive).unwrap_or_else(|_| EnvFilter::new("warn"));
    match file_appender() {
        Some(appender) => install(appender, filter),
        None => install(std::io::stderr, filter),
    }
}

fn install<W>(writer: W, filter: EnvFilter)
where
    W: for<'writer> tracing_subscriber::fmt::writer::MakeWriter<'writer> + Send + Sync + 'static,
{
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_flag_wins_over_env_and_default() {
        assert_eq!(level_directive_for(true, Some("off")), "debug");
        assert_eq!(level_directive_for(true, None), "debug");
    }

    #[test]
    fn env_value_wins_over_default_and_is_trimmed() {
        assert_eq!(level_directive_for(false, Some(" info ")), "info");
        assert_eq!(level_directive_for(false, None), "warn");
        assert_eq!(level_directive_for(false, Some("")), "warn");
    }

    #[test]
    fn unparsable_env_directive_falls_back_to_warn() {
        // A bare word is a legal target directive; only malformed
        // `target=level` pairs make EnvFilter reject the string, and
        // `init` then falls back to `warn`.
        assert!(EnvFilter::try_new("nonsense=verbose").is_err());
    }
}
