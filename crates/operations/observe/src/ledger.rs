/*!
 * @file MetricsLedger
 * @description Append-only store for per-turn metric samples.
 *
 * Responsibilities:
 * - Append one JSON line per finished turn under the home metrics dir.
 * - Read the ledger back, tolerating and counting malformed lines.
 * - Report the storage path so callers (and `doctor`) can point at it.
 *
 * The fold that produces a [`TurnSample`] lives in the event tap; this
 * module only moves samples between memory and disk. It must not depend on:
 * the actor, tools, policy, or any frontend.
 */

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::Metrics;

/// Directory name under the WaveCode home directory.
const METRICS_DIR: &str = "metrics";
/// Ledger file name inside that directory.
const LEDGER_FILE: &str = "turns.jsonl";

/// One turn's folded counters plus the attribution a fold cannot see.
///
/// `model` is stamped by the writer rather than read from the event stream:
/// the wire carries no model identity on every event, and `/model` can
/// switch it mid-session.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnSample {
    /// Unix seconds at write time.
    pub ts_secs: u64,
    /// Session the turn ran in.
    pub session: String,
    /// Model sampled for this turn.
    pub model: String,
    /// Counters folded from the turn's events.
    #[serde(default)]
    pub metrics: Metrics,
}

/// A ledger file handle bound to one path.
#[derive(Debug, Clone)]
pub struct Ledger {
    path: PathBuf,
}

impl Ledger {
    /// Ledger at `path` (created on first append).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Ledger under `<home>/.wavecode/metrics/turns.jsonl`.
    pub fn in_home(home: &Path) -> Self {
        Self::new(home.join(".wavecode").join(METRICS_DIR).join(LEDGER_FILE))
    }

    /// Where samples are stored, for status and doctor output.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one sample as a single JSON line.
    ///
    /// IO errors surface to the caller: a metric that never landed must not
    /// be silently missing from a baseline.
    pub fn append(&self, sample: &TurnSample) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(sample)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        file.flush()
    }

    /// Read every sample, newest order preserved.
    ///
    /// Blank lines are ignored; a line that fails to parse is counted and
    /// skipped, because one torn write at the tail must not hide the whole
    /// history. The count lets callers disclose the loss instead of
    /// presenting a silently shorter baseline.
    pub fn read(&self) -> LedgerRead {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LedgerRead::default(),
            Err(_) => return LedgerRead::default(),
        };
        let mut samples = Vec::new();
        let mut malformed = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<TurnSample>(line) {
                Ok(sample) => samples.push(sample),
                Err(_) => malformed += 1,
            }
        }
        LedgerRead { samples, malformed }
    }
}

/// Outcome of a ledger read.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LedgerRead {
    /// Samples that parsed.
    pub samples: Vec<TurnSample>,
    /// Lines skipped as unreadable.
    pub malformed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(session: &str, model: &str, calls: u64) -> TurnSample {
        let mut metrics = Metrics::new();
        metrics.tool_calls = calls;
        TurnSample {
            ts_secs: 1,
            session: session.to_string(),
            model: model.to_string(),
            metrics,
        }
    }

    #[test]
    fn appends_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::in_home(dir.path());
        assert_eq!(ledger.read(), LedgerRead::default());

        ledger.append(&sample("s1", "opus", 3)).unwrap();
        ledger.append(&sample("s1", "sonnet", 5)).unwrap();
        let read = ledger.read();
        assert_eq!(read.malformed, 0);
        assert_eq!(read.samples.len(), 2);
        assert_eq!(read.samples[1].model, "sonnet");
        assert_eq!(read.samples[1].metrics.tool_calls, 5);
    }

    /// A torn tail line costs that line, not the history.
    #[test]
    fn skips_and_counts_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::in_home(dir.path());
        ledger.append(&sample("s1", "opus", 2)).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(ledger.path())
            .and_then(|mut f| writeln!(f, "{{\"truncated\":"))
            .unwrap();

        let read = ledger.read();
        assert_eq!(read.samples.len(), 1);
        assert_eq!(read.malformed, 1);
    }

    #[test]
    fn merges_samples_into_totals() {
        let mut total = Metrics::new();
        total.merge(&sample("a", "opus", 2).metrics);
        total.merge(&sample("b", "opus", 3).metrics);
        assert_eq!(total.tool_calls, 5);
    }
}
