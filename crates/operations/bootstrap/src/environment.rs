/*!
 * @file EnvironmentFacts
 * @description Environment section for the system prompt.
 *
 * Responsibilities:
 * - Describe the host (OS, arch, shell), serving model, working directory,
 *   and session date as a stable, factual paragraph for the `environment`
 *   prompt slot.
 * - Render the date through `infrastructure-base`, the single implementation
 *   shared with the loop's midnight-rollover notice.
 *
 * This module must not depend on: runtime, state, transport, or tools.
 */

//! Host facts for the system prompt's `# Environment` section.

use std::path::Path;
use std::time::SystemTime;

/// Build the environment paragraph shown to the model.
///
/// `now` seeds the session date, which is frozen per assembly: the paragraph
/// is a stable prefix (prompt caching depends on it). A session that outlives
/// midnight is covered by the loop's date-change reminder, which announces the
/// new date in-history instead of rewriting this section. The same freeze
/// applies to `model` and `context_window`: a mid-session `/model` switch
/// changes the loop's live window but not this line. `cwd` renders as given;
/// an empty string skips the line.
pub fn describe(cwd: &Path, now: SystemTime, model: &str, context_window: u64) -> String {
    let mut lines = vec![
        format!("OS: {} ({})", std::env::consts::OS, std::env::consts::ARCH),
        format!("Shell: {}", shell_name()),
    ];
    if !cwd.as_os_str().is_empty() {
        lines.push(format!("Working directory: {}", cwd.display()));
    }
    lines.push(format!(
        "Model: {model} (context window: {context_window} tokens)"
    ));
    lines.push(format!("Session date: {}", format_date(now)));
    lines.join("\n")
}

/// Best-effort shell name: `$SHELL` basename on Unix, `%COMSPEC%` basename
/// (defaulting to cmd.exe) on Windows, "unknown" when nothing is set.
fn shell_name() -> String {
    let var = if cfg!(windows) { "COMSPEC" } else { "SHELL" };
    std::env::var(var)
        .ok()
        .and_then(|path| {
            Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "cmd.exe".into()
            } else {
                "unknown".into()
            }
        })
}

/// Format `now` as `YYYY-MM-DD (Weekday)`; see
/// [`infrastructure_base::format_date`] for the shared implementation.
pub use infrastructure_base::format_date;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn epoch_days(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(days * 86_400)
    }

    #[test]
    fn describe_lists_os_shell_model_cwd_date() {
        let text = describe(
            Path::new("/tmp/project"),
            epoch_days(0),
            "demo-model",
            200_000,
        );
        assert!(text.contains("OS: "), "{text}");
        assert!(text.contains("Shell: "), "{text}");
        assert!(text.contains("Working directory: /tmp/project"), "{text}");
        assert!(
            text.contains("Model: demo-model (context window: 200000 tokens)"),
            "{text}"
        );
        assert!(text.contains("Session date: 1970-01-01"), "{text}");
    }

    #[test]
    fn describe_skips_empty_cwd() {
        let text = describe(Path::new(""), epoch_days(0), "demo-model", 200_000);
        assert!(!text.contains("Working directory"), "{text}");
    }
}
