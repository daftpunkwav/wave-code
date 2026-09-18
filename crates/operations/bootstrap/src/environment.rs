/*!
 * @file EnvironmentFacts
 * @description Environment section for the system prompt.
 *
 * Responsibilities:
 * - Describe the host (OS, arch, shell), working directory, and session
 *   date as a stable, factual paragraph for the `environment` prompt slot.
 * - Derive the calendar date from `SystemTime` with no external time
 *   dependency (civil-from-days algorithm, unit-tested against known dates).
 *
 * This module must not depend on: runtime, state, transport, or tools.
 */

//! Host facts for the system prompt's `# Environment` section.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Build the environment paragraph shown to the model.
///
/// `now` seeds the session date; the value is frozen per assembly, so a
/// session crossing midnight keeps its start date (the same trade kimi
/// avoids only through a runtime reminder channel WaveCode does not wire
/// yet). `cwd` renders as given; an empty string skips the line.
pub fn describe(cwd: &Path, now: SystemTime) -> String {
    let mut lines = vec![
        format!("OS: {} ({})", std::env::consts::OS, std::env::consts::ARCH),
        format!("Shell: {}", shell_name()),
    ];
    if !cwd.as_os_str().is_empty() {
        lines.push(format!("Working directory: {}", cwd.display()));
    }
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

/// Format `now` as `YYYY-MM-DD (Weekday)` in UTC.
///
/// The session prompt carries one date, so UTC-vs-local skew only matters
/// around midnight and is accepted in exchange for staying dependency-free.
pub fn format_date(now: SystemTime) -> String {
    let days = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(days as i64);
    let weekday = WEEKDAYS[((days as i64 + 4).rem_euclid(7)) as usize];
    format!("{year:04}-{month:02}-{day:02} ({weekday})")
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Convert days since 1970-01-01 to a proleptic Gregorian (year, month,
/// day); Howard Hinnant's civil-from-days algorithm, valid for negative
/// day counts too.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn epoch_days(days: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(days * 86_400)
    }

    #[test]
    fn date_formats_known_days() {
        // 1970-01-01 was a Thursday.
        assert_eq!(format_date(epoch_days(0)), "1970-01-01 (Thursday)");
        // 2026-09-19 was a Saturday.
        assert_eq!(format_date(epoch_days(20_715)), "2026-09-19 (Saturday)");
        // 2000-02-29 (leap day, a Tuesday).
        assert_eq!(format_date(epoch_days(11_016)), "2000-02-29 (Tuesday)");
    }

    #[test]
    fn civil_from_days_handles_year_boundaries() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(364), (1970, 12, 31));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        // Before the epoch stays correct (negative days).
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn describe_lists_os_shell_cwd_date() {
        let text = describe(Path::new("/tmp/project"), epoch_days(0));
        assert!(text.contains("OS: "), "{text}");
        assert!(text.contains("Shell: "), "{text}");
        assert!(text.contains("Working directory: /tmp/project"), "{text}");
        assert!(text.contains("Session date: 1970-01-01"), "{text}");
    }

    #[test]
    fn describe_skips_empty_cwd() {
        let text = describe(Path::new(""), epoch_days(0));
        assert!(!text.contains("Working directory"), "{text}");
    }
}
