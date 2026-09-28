/*!
 * @file RuntimeBase
 * @description OS runtime primitives: channels, interruption, limits, and
 *   the shared calendar-date and command-shell resolution.
 *
 * Responsibilities:
 * - Centralize channel capacities and timeout constants.
 * - Provide the cooperative interrupt handle shared across tasks.
 * - Document truncation budgets for event payloads.
 * - Render the calendar date once for every layer that needs it (prompt
 *   assembly and the loop's midnight-rollover notice).
 * - Resolve the platform command-string shell once for every layer that
 *   spawns one (shell tool, PTY shell, hooks, jobs).
 * - Own the crash-safe file-replace primitive shared by the durable stores.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Runtime base primitives with centralized limits.
//!
//! Scattered magic numbers are a maintenance hazard, so every capacity,
//! timeout, and truncation budget lives here with its rationale.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Capacity of the main event channel per session.
pub const EVENT_CHANNEL_CAP: usize = 256;
/// Capacity of the control channel for interrupts and approvals.
pub const CONTROL_CHANNEL_CAP: usize = 32;
/// Maximum queued submissions per session; overflow is an explicit error.
pub const PENDING_QUEUE_CAP: usize = 64;
/// Grace period for draining in-flight work on shutdown.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);
/// Poll interval while a run parks on an approval decision.
pub const APPROVAL_POLL: Duration = Duration::from_millis(25);
/// Default timeout for lifecycle hook commands.
pub const HOOK_DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Character budget for event text payloads.
pub const EVENT_TEXT_TRUNCATION: usize = 2000;
/// Character budget for approval detail strings; sized for the
/// multi-line diff a file-write approval carries.
pub const APPROVAL_DETAIL_TRUNCATION: usize = 2000;

/// Cooperative interrupt handle shared across spawned tasks.
///
/// Interrupts are delivered at safe points only: tasks poll
/// [`InterruptHandle::is_triggered`] between units of work instead of being
/// cancelled mid-operation, so no half-written state is left behind.
#[derive(Debug, Clone, Default)]
pub struct InterruptHandle {
    flag: Arc<AtomicBool>,
}

impl InterruptHandle {
    /// Create an untriggered handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal interruption; safe to call from any thread.
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// True after [`InterruptHandle::trigger`] until [`InterruptHandle::reset`].
    pub fn is_triggered(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Clear a previous trigger, e.g. when a new run starts.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }
}

/// Truncate text to a character budget, appending an ellipsis on cut.
///
/// Operates on `char` boundaries so truncation never splits UTF-8.
/// A zero budget yields an empty string rather than a bare ellipsis.
pub fn truncate(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().take(max_chars).collect();
    format!("{kept}...")
}

/// Pick the shell program and its "run a command string" flag.
///
/// One resolution for every layer that spawns a command string (the shell
/// tool, the PTY shell, lifecycle hooks, background jobs), so the
/// `WAVECODE_SHELL` override can never be honored by one spawn path and
/// silently ignored by another.
///
/// Platform default: `cmd /C` on Windows, `sh -c` elsewhere. Setting
/// `WAVECODE_SHELL` overrides the program (the value is the program path);
/// the flag style is then a heuristic — values containing `cmd` use `/C`,
/// everything else uses `-c`. That covers common names (cmd, powershell,
/// bash, zsh) but may guess wrong for unusual ones; keep it simple and
/// switch to explicit configuration only when a real shell demands it.
pub fn shell_invocation() -> (String, &'static str) {
    if let Ok(custom) = std::env::var("WAVECODE_SHELL") {
        if custom.to_lowercase().contains("cmd") {
            return (custom, "/C");
        }
        return (custom, "-c");
    }
    if cfg!(windows) {
        ("cmd".to_owned(), "/C")
    } else {
        ("sh".to_owned(), "-c")
    }
}

/// Atomic file replace: write `contents` to a unique sibling staging file,
/// then rename it over `path`, so a crash mid-write leaves the target with
/// either the old or the new content — never a half-written mix.
///
/// The staging name carries the process id plus a per-process sequence
/// number: a fixed staging name lets two writers clobber each other's bytes
/// mid-write and rename half a file into place. A failed rename removes the
/// staging file best-effort (no litter; the error still propagates). Sync
/// `std::fs` only — stores needing stronger durability (fsync before the
/// rename) keep their own write path on purpose.
pub fn atomic_write(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let staging = match path.extension() {
        Some(ext) => path.with_extension(format!(
            "{}.staging-{}-{}",
            ext.to_string_lossy(),
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )),
        None => path.with_extension(format!(
            "staging-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        )),
    };
    std::fs::write(&staging, contents)?;
    if let Err(e) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(e);
    }
    Ok(())
}

/// Weekday names indexed by `days_since_epoch % 7` with 1970-01-01 =
/// Thursday (index 4).
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Render `now` as `YYYY-MM-DD (Weekday)` in UTC.
///
/// One rendering for every layer that shows a date (the prompt's environment
/// section and the loop's midnight-rollover notice), so the two can never
/// disagree. UTC-vs-local skew only matters around midnight and is accepted
/// in exchange for staying dependency-free.
pub fn format_date(now: std::time::SystemTime) -> String {
    // Whole days since the epoch, rounding toward negative infinity so a
    // pre-epoch clock (a machine with an unset or badly set RTC) renders the
    // real calendar day instead of silently clamping to 1970-01-01.
    let days: i64 = match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => (since.as_secs() / 86_400) as i64,
        Err(before) => {
            let secs = before.duration().as_secs();
            // Ceil, then negate: 1 s before the epoch is 1969-12-31, not -0.
            -(secs.div_ceil(86_400) as i64)
        }
    };
    let (year, month, day) = civil_from_days(days);
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    format!("{year:04}-{month:02}-{day:02} ({weekday})")
}

/// Render epoch seconds as `YYYY-MM-DD HH:MM UTC`.
///
/// One rendering for every layer that shows a wall-clock instant (persisted
/// tool-result headers), sharing the civil-date math with [`format_date`].
/// UTC-only matches [`format_date`]: local-offset rendering would need a
/// timezone database, which the dependency-free date formatting here
/// deliberately avoids.
pub fn format_timestamp(secs: u64) -> String {
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        tod / 3_600,
        (tod % 3_600) / 60
    )
}

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
    use std::sync::Mutex;

    #[test]
    fn interrupt_handle_triggers_and_resets() {
        let handle = InterruptHandle::new();
        assert!(!handle.is_triggered());
        handle.trigger();
        assert!(handle.is_triggered());
        // Clones share the same flag.
        assert!(handle.clone().is_triggered());
        handle.reset();
        assert!(!handle.is_triggered());
    }

    #[test]
    fn truncation_keeps_short_text_and_cuts_long_text() {
        assert_eq!(truncate("abc", 10), "abc");
        let cut = truncate("abcdef", 3);
        assert_eq!(cut, "abc...");
    }

    #[test]
    fn truncation_zero_budget_returns_empty() {
        assert_eq!(truncate("abcdef", 0), "");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncation_cuts_on_char_boundaries() {
        assert_eq!(
            truncate("\u{65e5}\u{672c}\u{8a9e}", 2),
            "\u{65e5}\u{672c}..."
        );
        assert_eq!(truncate("a\u{1f642}b", 2), "a\u{1f642}...");
    }

    /// Serializes tests that mutate the process-global `WAVECODE_SHELL`
    /// variable; parallel env mutation would race between threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn shell_invocation_defaults_to_the_platform_shell() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("WAVECODE_SHELL").ok();
        unsafe {
            std::env::remove_var("WAVECODE_SHELL");
        }
        let (program, flag) = shell_invocation();
        unsafe {
            if let Some(v) = prior {
                std::env::set_var("WAVECODE_SHELL", v);
            }
        }
        if cfg!(windows) {
            assert_eq!((program.as_str(), flag), ("cmd", "/C"));
        } else {
            assert_eq!((program.as_str(), flag), ("sh", "-c"));
        }
    }

    #[test]
    fn shell_invocation_override_picks_flag_style_by_name() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var("WAVECODE_SHELL").ok();
        // A cmd-like name keeps the Windows command-string flag.
        unsafe {
            std::env::set_var("WAVECODE_SHELL", r"C:\tools\my-cmd.exe");
        }
        let (program, flag) = shell_invocation();
        assert_eq!(program, r"C:\tools\my-cmd.exe");
        assert_eq!(flag, "/C");
        // Any other name gets the Unix command-string flag.
        unsafe {
            std::env::set_var("WAVECODE_SHELL", "/usr/bin/zsh");
        }
        let (program, flag) = shell_invocation();
        assert_eq!(program, "/usr/bin/zsh");
        assert_eq!(flag, "-c");
        unsafe {
            std::env::remove_var("WAVECODE_SHELL");
            if let Some(v) = prior {
                std::env::set_var("WAVECODE_SHELL", v);
            }
        }
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_staging_litter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("store.json");
        atomic_write(&path, b"first").expect("first write");
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        // An overwrite swaps wholesale: the destination always holds the
        // old or the new content, and the staging file is gone once the
        // rename lands.
        atomic_write(&path, b"second").expect("overwrite");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let litter: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".staging-"))
            .collect();
        assert!(litter.is_empty(), "staging files must not linger: {litter:?}");
    }

    /// A failed rename must clean up its staging file instead of littering
    /// the store directory: callers fail loudly with the destination left
    /// untouched.
    #[test]
    fn failed_atomic_write_removes_its_staging_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("store.json");
        // A directory at the destination makes the final rename fail on
        // every platform (a file cannot replace a directory).
        std::fs::create_dir(&path).expect("blocker dir");
        assert!(
            atomic_write(&path, b"new").is_err(),
            "renaming onto a directory must fail"
        );
        let litter: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".staging-"))
            .collect();
        assert!(litter.is_empty(), "the staging file must be removed: {litter:?}");
    }

    #[test]
    fn date_renders_known_days() {
        let days = |n: u64| std::time::UNIX_EPOCH + Duration::from_secs(n * 86_400);
        // 1970-01-01 was a Thursday.
        assert_eq!(format_date(days(0)), "1970-01-01 (Thursday)");
        // 2026-09-19 was a Saturday.
        assert_eq!(format_date(days(20_715)), "2026-09-19 (Saturday)");
        // 2000-02-29 (leap day, a Tuesday).
        assert_eq!(format_date(days(11_016)), "2000-02-29 (Tuesday)");
        // Year boundaries stay contiguous.
        assert_eq!(format_date(days(364)), "1970-12-31 (Thursday)");
        assert_eq!(format_date(days(365)), "1971-01-01 (Friday)");
    }

    #[test]
    fn date_before_the_epoch_stays_correct() {
        // 1969-12-31 was a Wednesday; a broken RTC must not render 1970.
        let before = std::time::UNIX_EPOCH - Duration::from_secs(86_400);
        assert_eq!(format_date(before), "1969-12-31 (Wednesday)");
        // One second before the epoch is still the previous day.
        let just_before = std::time::UNIX_EPOCH - Duration::from_secs(1);
        assert_eq!(format_date(just_before), "1969-12-31 (Wednesday)");
        // Exactly the epoch boundary.
        assert_eq!(format_date(std::time::UNIX_EPOCH), "1970-01-01 (Thursday)");
    }

    #[test]
    fn timestamp_renders_known_instants() {
        // 1970-01-01T00:00:00Z.
        assert_eq!(format_timestamp(0), "1970-01-01 00:00 UTC");
        // 2026-09-19T18:03:00Z: minutes and hours render padded.
        assert_eq!(
            format_timestamp(20_715 * 86_400 + 18 * 3_600 + 3 * 60),
            "2026-09-19 18:03 UTC"
        );
        // End of day rolls into the next day's date.
        assert_eq!(
            format_timestamp(20_715 * 86_400 + 86_399),
            "2026-09-19 23:59 UTC"
        );
        assert_eq!(format_timestamp(20_716 * 86_400), "2026-09-20 00:00 UTC");
    }
}
