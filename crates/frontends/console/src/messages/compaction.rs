/*!
 * @file CompactionCard
 * @description Live transcript card for context compaction.
 *
 * Responsibilities:
 * - Render the running compaction with a saw-wave pulse and elapsed time.
 * - Finalize in place with the summary size and the context usage it replaced.
 *
 * This module must not depend on: runtime, capability, or actor crates.
 */

//! The compaction transcript card: live pulse while summarizing, then a
//! settled summary line.

use std::time::Instant;

use crate::state::format_tokens;
use crate::theme::{self, Token};
use tui_engine::component::Component;
use tui_engine::loader::SAW_FRAMES;
use tui_engine::width::strip_ansi;

/// One compaction run: created live on `CompactStarted`, finalized in
/// place on `CompactCompleted`.
pub struct CompactionCard {
    trigger: String,
    started: Instant,
    /// `None` while running; the settled outcome afterwards.
    finished: Option<CompactionStats>,
}

/// What the completed compaction produced.
#[derive(Debug, Clone, Copy)]
pub struct CompactionStats {
    /// Token estimate of the summary message.
    pub summary_tokens: u64,
    /// Context usage at the moment compaction started (the most
    /// recent sample); `None` before any sample settled.
    pub context_before: Option<u64>,
}

impl CompactionCard {
    /// A live card for one compaction run.
    pub fn running(trigger: &str) -> Self {
        Self {
            trigger: trigger.to_string(),
            started: Instant::now(),
            finished: None,
        }
    }

    /// Finalize with the summary tokens and the pre-compaction usage.
    pub fn finish(&mut self, summary_tokens: u64, context_before: Option<u64>) {
        self.finished = Some(CompactionStats {
            summary_tokens,
            context_before,
        });
    }

    /// True while the compaction is still running.
    pub fn is_running(&self) -> bool {
        self.finished.is_none()
    }
}

impl Component for CompactionCard {
    fn render(&mut self, _columns: usize) -> Vec<String> {
        let theme = theme::current();
        let trigger = strip_ansi(&self.trigger);
        match self.finished {
            None => {
                let step = (self.started.elapsed().as_millis()
                    / tui_engine::loader::SAW_INTERVAL_MS as u128) as usize;
                let frame = SAW_FRAMES[step % SAW_FRAMES.len()];
                let seconds = self.started.elapsed().as_secs();
                vec![theme.paint(
                    Token::TextDim,
                    &format!("{frame} compacting context ({trigger})… {seconds}s"),
                )]
            }
            Some(stats) => {
                let before = stats
                    .context_before
                    .map(|tokens| format_tokens(tokens))
                    .unwrap_or_else(|| "?".to_string());
                vec![theme.paint(
                    Token::Success,
                    &format!(
                        "● compacted ({trigger}): context {before}, summary {} tokens",
                        format_tokens(stats.summary_tokens)
                    ),
                )]
            }
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tui_engine::width::strip_ansi;

    #[test]
    fn live_card_pulse_mentions_trigger_and_elapsed() {
        let mut card = CompactionCard::running("manual");
        assert!(card.is_running());
        let lines = card.render(60);
        let plain = strip_ansi(&lines[0]);
        assert!(plain.contains("compacting context (manual)"), "{plain}");
        assert!(plain.contains('▁') || plain.contains('▃') || plain.contains('▅') || plain.contains('▇'), "saw frame: {plain}");
        assert!(plain.contains('s'), "elapsed seconds: {plain}");
    }

    #[test]
    fn finished_card_reports_before_and_summary() {
        let mut card = CompactionCard::running("manual");
        card.finish(3_300, Some(84_000));
        assert!(!card.is_running());
        let lines = card.render(120);
        let plain = strip_ansi(&lines[0]);
        assert!(plain.contains("compacted (manual)"), "{plain}");
        assert!(plain.contains("context 82.0k"), "{plain}");
        assert!(plain.contains("summary 3.2k tokens"), "{plain}");
    }

    #[test]
    fn finished_without_usage_shows_placeholder() {
        let mut card = CompactionCard::running("auto");
        card.finish(1_500, None);
        let plain = strip_ansi(&card.render(120).join("\n"));
        assert!(plain.contains("context ?"), "{plain}");
    }
}
