/*!
 * @file TaskScheduler
 * @description Priority queues, delayed tasks, cron matching, limits.
 *
 * Responsibilities:
 * - Order work by priority with FIFO stability inside each level.
 * - Hold delayed tasks until their deadline passes.
 * - Match five-field cron expressions against civil time.
 * - Cap concurrent holders with semaphore guards.
 *
 * This module must not depend on: any other workspace crate.
 */

//! Scheduling primitives with explicit clocks.
//!
//! Time enters as parameters (epoch seconds, civil fields), never as hidden
//! reads, so every schedule is deterministic under test.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Scheduling priority; higher levels always drain first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// Background work.
    Low,
    /// Default work.
    Normal,
    /// User-facing work.
    High,
}

/// One queued task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedTask {
    /// Stable identifier.
    pub id: String,
    /// Scheduling priority.
    pub priority: Priority,
    /// Human-readable label for observability.
    pub label: String,
}

/// Priority queue with FIFO stability inside each level.
#[derive(Debug, Default)]
pub struct TaskQueue {
    high: VecDeque<QueuedTask>,
    normal: VecDeque<QueuedTask>,
    low: VecDeque<QueuedTask>,
}

impl TaskQueue {
    /// Create an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one task to its level tail.
    pub fn push(&mut self, task: QueuedTask) {
        match task.priority {
            Priority::High => self.high.push_back(task),
            Priority::Normal => self.normal.push_back(task),
            Priority::Low => self.low.push_back(task),
        }
    }

    /// Pop the head of the highest non-empty level.
    pub fn pop(&mut self) -> Option<QueuedTask> {
        self.high
            .pop_front()
            .or_else(|| self.normal.pop_front())
            .or_else(|| self.low.pop_front())
    }

    /// Total queued tasks across levels.
    pub fn len(&self) -> usize {
        self.high.len() + self.normal.len() + self.low.len()
    }

    /// True when every level is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One delayed task firing no earlier than its deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelayedTask {
    /// Stable identifier.
    pub id: String,
    /// Epoch seconds at or after which the task is due.
    pub not_before: u64,
}

/// Delay queue drained by explicit clock reads.
#[derive(Debug, Default)]
pub struct DelayQueue {
    tasks: Vec<DelayedTask>,
}

impl DelayQueue {
    /// Create an empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Hold one task until its deadline.
    pub fn push(&mut self, task: DelayedTask) {
        self.tasks.push(task);
    }

    /// Take all tasks due at `now_secs`, keeping future ones queued.
    pub fn pop_due(&mut self, now_secs: u64) -> Vec<DelayedTask> {
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.tasks)
            .into_iter()
            .partition(|t| t.not_before <= now_secs);
        self.tasks = later;
        due
    }

    /// Number of still-waiting tasks.
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// True when no task is waiting.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

/// One cron field: any value, or an exact match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CronField {
    /// Wildcard: matches every value.
    Any,
    /// Exact value match.
    Exact(u8),
}

impl CronField {
    /// Parse one field: `*` or a plain integer.
    pub fn parse(text: &str) -> Result<Self, ScheduleError> {
        if text == "*" {
            return Ok(Self::Any);
        }
        text.parse::<u8>()
            .map(Self::Exact)
            .map_err(|_| ScheduleError::InvalidCron(text.to_string()))
    }

    /// Exact value when this field is not a wildcard.
    pub fn exact_value(&self) -> Option<u8> {
        match self {
            Self::Any => None,
            Self::Exact(value) => Some(*value),
        }
    }

    /// True when `value` satisfies this field.
    pub fn matches(&self, value: u8) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(want) => *want == value,
        }
    }
}

/// Civil time for cron matching, supplied by the caller clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilTime {
    /// Minute of hour, 0-59.
    pub minute: u8,
    /// Hour of day, 0-23.
    pub hour: u8,
    /// Day of month, 1-31.
    pub day: u8,
    /// Month of year, 1-12.
    pub month: u8,
    /// Day of week, 0-6 starting Sunday.
    pub weekday: u8,
}

/// Five-field cron expression (minute hour day month weekday).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CronSpec {
    /// Minute field.
    pub minute: CronField,
    /// Hour field.
    pub hour: CronField,
    /// Day-of-month field.
    pub day: CronField,
    /// Month field.
    pub month: CronField,
    /// Day-of-week field.
    pub weekday: CronField,
}

impl CronSpec {
    /// Parse `minute hour day month weekday` with `*` wildcards.
    ///
    /// Exact values must name real moments (minute 0-59, hour 0-23, day
    /// 1-31, month 1-12, weekday 0-6): out-of-range fields parse as
    /// integers but can never match a valid [`CivilTime`], so accepting
    /// them would arm a dead schedule that silently never fires.
    pub fn parse(text: &str) -> Result<Self, ScheduleError> {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(ScheduleError::InvalidCron(text.to_string()));
        }
        let spec = Self {
            minute: CronField::parse(parts[0])?,
            hour: CronField::parse(parts[1])?,
            day: CronField::parse(parts[2])?,
            month: CronField::parse(parts[3])?,
            weekday: CronField::parse(parts[4])?,
        };
        let ranges = [
            (spec.minute, 0, 59),
            (spec.hour, 0, 23),
            (spec.day, 1, 31),
            (spec.month, 1, 12),
            (spec.weekday, 0, 6),
        ];
        let in_range = ranges.iter().all(|(field, lo, hi)| {
            field
                .exact_value()
                .is_none_or(|value| value >= *lo && value <= *hi)
        });
        if !in_range {
            return Err(ScheduleError::InvalidCron(text.to_string()));
        }
        Ok(spec)
    }

    /// True when every field matches the given civil time.
    pub fn matches(&self, time: CivilTime) -> bool {
        self.minute.matches(time.minute)
            && self.hour.matches(time.hour)
            && self.day.matches(time.day)
            && self.month.matches(time.month)
            && self.weekday.matches(time.weekday)
    }
}

/// Scheduling errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    /// A cron expression or field failed to parse.
    #[error("invalid cron expression: {0}")]
    InvalidCron(String),
}

/// Edge-triggered cron driver: fires once per matching minute.
///
/// The driver owns no clock; callers tick it from their own loop with the
/// current civil time. Repeated ticks inside one minute fire once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronDaemon {
    spec: CronSpec,
    last_fired: Option<CivilTime>,
}

impl CronDaemon {
    /// Watch one cron expression.
    pub fn new(spec: CronSpec) -> Self {
        Self {
            spec,
            last_fired: None,
        }
    }

    /// Tick with the current time; true exactly once per matching minute.
    pub fn tick(&mut self, time: CivilTime) -> bool {
        if !self.spec.matches(time) {
            return false;
        }
        // The whole timestamp is the edge key: any field differing means a
        // different minute, so matching minutes in different months (or on
        // different weekdays) each fire instead of sharing one dedup slot.
        if self.last_fired == Some(time) {
            return false;
        }
        self.last_fired = Some(time);
        true
    }
}

/// Concurrency gate: at most `limit` holders proceed at once.
#[derive(Debug, Clone)]
pub struct ConcurrencyLimit {
    semaphore: Arc<Semaphore>,
}

impl ConcurrencyLimit {
    /// Create a gate admitting `limit` concurrent holders.
    pub fn new(limit: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(limit.max(1))),
        }
    }

    /// Acquire one permit, waiting while the gate is full.
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        self.semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closes")
    }

    /// Permits currently available without waiting.
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, priority: Priority) -> QueuedTask {
        QueuedTask {
            id: id.to_string(),
            priority,
            label: id.to_string(),
        }
    }

    #[test]
    fn priority_drains_high_first_with_fifo_levels() {
        let mut queue = TaskQueue::new();
        queue.push(task("low1", Priority::Low));
        queue.push(task("high1", Priority::High));
        queue.push(task("normal1", Priority::Normal));
        queue.push(task("high2", Priority::High));
        let order: Vec<_> = std::iter::from_fn(|| queue.pop()).map(|t| t.id).collect();
        assert_eq!(order, vec!["high1", "high2", "normal1", "low1"]);
    }

    #[test]
    fn delay_queue_releases_only_due_tasks() {
        let mut queue = DelayQueue::new();
        queue.push(DelayedTask {
            id: "a".to_string(),
            not_before: 10,
        });
        queue.push(DelayedTask {
            id: "b".to_string(),
            not_before: 20,
        });
        let due = queue.pop_due(15);
        assert_eq!(
            due.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn cron_wildcard_matches_and_exact_filters() {
        let any = CronSpec::parse("* * * * *").unwrap();
        let nine = CronSpec::parse("0 9 * * *").unwrap();
        let morning = CivilTime {
            minute: 0,
            hour: 9,
            day: 1,
            month: 1,
            weekday: 1,
        };
        let evening = CivilTime {
            minute: 0,
            hour: 21,
            day: 1,
            month: 1,
            weekday: 1,
        };
        assert!(any.matches(morning));
        assert!(any.matches(evening));
        assert!(nine.matches(morning));
        assert!(!nine.matches(evening));
        assert!(CronSpec::parse("0 9").is_err());
    }

    #[test]
    fn cron_rejects_out_of_range_fields() {
        for bad in [
            "99 9 * * *", // minute
            "0 24 * * *", // hour
            "0 9 0 * *",  // day zero
            "0 9 32 * *", // day overflow
            "0 9 * 0 *",  // month zero
            "0 9 * 13 *", // month overflow
            "0 9 * * 7",  // weekday overflow
            "99 99 99 99 99",
        ] {
            assert!(CronSpec::parse(bad).is_err(), "{bad} should not parse");
        }
        // Boundaries and wildcards still parse.
        for good in ["0 0 1 1 0", "59 23 31 12 6", "* * * * *", "0 9 * * *"] {
            assert!(CronSpec::parse(good).is_ok(), "{good} should parse");
        }
    }

    #[test]
    fn cron_daemon_fires_once_per_matching_minute() {
        let mut daemon = CronDaemon::new(CronSpec::parse("0 9 * * *").unwrap());
        let morning = CivilTime {
            minute: 0,
            hour: 9,
            day: 1,
            month: 1,
            weekday: 1,
        };
        assert!(daemon.tick(morning));
        assert!(!daemon.tick(morning));
        let evening = CivilTime {
            minute: 0,
            hour: 21,
            day: 1,
            month: 1,
            weekday: 1,
        };
        assert!(!daemon.tick(evening));
    }

    #[test]
    fn cron_daemon_fires_matching_minutes_across_months() {
        let mut daemon = CronDaemon::new(CronSpec::parse("0 9 1 * *").unwrap());
        let january = CivilTime {
            minute: 0,
            hour: 9,
            day: 1,
            month: 1,
            weekday: 3,
        };
        let february = CivilTime {
            month: 2,
            weekday: 6,
            ..january
        };
        assert!(daemon.tick(january));
        assert!(!daemon.tick(january));
        // Same day/hour/minute in another month is another minute.
        assert!(daemon.tick(february));
        assert!(!daemon.tick(february));
    }

    #[tokio::test]
    async fn concurrency_gate_caps_holders() {
        let gate = ConcurrencyLimit::new(1);
        let _first = gate.acquire().await;
        assert_eq!(gate.available(), 0);
        drop(_first);
        assert_eq!(gate.available(), 1);
    }
}
