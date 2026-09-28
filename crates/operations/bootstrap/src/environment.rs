/*!
 * @file EnvironmentFacts
 * @description Environment section for the system prompt.
 *
 * Responsibilities:
 * - Describe the host (OS, arch, shell) and working directory as a stable,
 *   factual paragraph for the `environment` prompt slot.
 *
 * This module must not depend on: runtime, state, transport, or tools.
 */

//! Host facts for the system prompt's `# Environment` section.

use std::path::Path;

/// Build the environment paragraph shown to the model.
///
/// Only session-stable facts live here: the paragraph is a frozen prefix
/// (prompt caching depends on it). Everything that can change mid-session —
/// the serving model, its live context window, today's date — is rendered
/// per sample into the loop's transient notes instead, so a `/model` switch
/// or a midnight rollover is correct on the very next request without
/// touching this section. `cwd` renders as given; an empty string skips
/// the line.
pub fn describe(cwd: &Path) -> String {
    let mut lines = vec![
        format!("OS: {} ({})", std::env::consts::OS, std::env::consts::ARCH),
        format!("Shell: {}", shell_name()),
    ];
    if !cwd.as_os_str().is_empty() {
        lines.push(format!("Working directory: {}", cwd.display()));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describe_lists_os_shell_cwd_only() {
        let text = describe(Path::new("/tmp/project"));
        assert!(text.contains("OS: "), "{text}");
        assert!(text.contains("Shell: "), "{text}");
        assert!(text.contains("Working directory: /tmp/project"), "{text}");
        // Session-variable facts never freeze here: model, window, and the
        // date render per sample into the loop's transient notes.
        assert!(!text.contains("Model"), "{text}");
        assert!(!text.contains("date"), "{text}");
    }

    #[test]
    fn describe_skips_empty_cwd() {
        let text = describe(Path::new(""));
        assert!(!text.contains("Working directory"), "{text}");
    }
}
