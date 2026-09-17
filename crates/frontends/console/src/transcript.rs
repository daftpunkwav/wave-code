/*!
 * @file Transcript
 * @description Turn-based visual transcript buffer for the interactive console.
 *
 * Responsibilities:
 * - Maintain ordered sequence of rendered message components.
 * - Group entries into discrete conversational turns.
 * - Support windowed trimming for bounded memory and clear operations.
 *
 * This module must not depend on: runtime, network, or actor internal state.
 */

//! The transcript: ordered message components grouped into turns, with
//! windowed trimming so long sessions stay bounded.
//!
//! A turn starts at a user message and owns everything until the next
//! one. Trimming keeps the most recent [`MAX_TURNS`] turns and only
//! fires beyond that plus [`HYSTERESIS`] turns, so bursts of activity
//! do not cause constant drops; the newest turn is never dropped.

use tui_engine::Component;

/// Turns kept after a trim.
pub const MAX_TURNS: usize = 15;
/// Extra turns tolerated before trimming kicks in.
pub const HYSTERESIS: usize = 5;

/// One transcript entry: a component plus its turn membership.
pub struct Entry {
    pub component: Box<dyn Component>,
    pub turn: usize,
}

/// The transcript container.
#[derive(Default)]
pub struct Transcript {
    entries: Vec<Entry>,
    /// Turn counter for the next entry (monotonic within a view lifetime;
    /// reset to zero by `clear` alongside the entries).
    next_turn: usize,
}

impl Transcript {
    /// An empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a component that continues the current turn.
    pub fn push(&mut self, component: Box<dyn Component>) {
        self.entries.push(Entry {
            component,
            turn: self.next_turn.saturating_sub(1),
        });
    }

    /// Append a component that STARTS a new turn.
    pub fn push_new_turn(&mut self, component: Box<dyn Component>) {
        self.entries.push(Entry {
            component,
            turn: self.next_turn,
        });
        self.next_turn += 1;
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Mutable entry access (live-streamed blocks mutate in place).
    pub fn get_mut(&mut self, index: usize) -> Option<&mut Entry> {
        self.entries.get_mut(index)
    }

    /// The last entry index, if any.
    pub fn last_index(&self) -> Option<usize> {
        self.entries.len().checked_sub(1)
    }

    /// Drop oldest turns once the count exceeds `MAX_TURNS +
    /// HYSTERESIS`, keeping the newest [`MAX_TURNS`] turns. Returns the
    /// number of entries dropped (callers adjust absolute indices).
    pub fn trim(&mut self) -> usize {
        if self.next_turn <= MAX_TURNS + HYSTERESIS {
            return 0;
        }
        let keep_from_turn = self.next_turn - MAX_TURNS;
        let keep = self
            .entries
            .iter()
            .position(|entry| entry.turn >= keep_from_turn)
            .unwrap_or(self.entries.len());
        self.entries.drain(..keep).count()
    }

    /// Render every entry to lines at `columns`.
    pub fn render(&mut self, columns: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for entry in &mut self.entries {
            lines.extend(entry.component.render(columns));
        }
        lines
    }

    /// Clear all transcript entries and reset the turn counter so the
    /// next view lifetime starts numbering from zero.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next_turn = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::StatusLine;
    use crate::theme;
    use tui_engine::width::strip_ansi;

    fn status(text: &str) -> Box<dyn Component> {
        Box::new(StatusLine::new(text))
    }

    #[test]
    fn entries_join_the_current_turn() {
        let mut transcript = Transcript::new();
        transcript.push_new_turn(status("user"));
        transcript.push(status("assistant"));
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript.entries[0].turn, 0);
        assert_eq!(transcript.entries[1].turn, 0);
        transcript.push_new_turn(status("user2"));
        assert_eq!(transcript.entries[2].turn, 1);
    }

    #[test]
    fn trim_drops_old_turns_with_hysteresis() {
        let mut transcript = Transcript::new();
        for turn in 0..=(MAX_TURNS + HYSTERESIS) {
            transcript.push_new_turn(status(&format!("u{turn}")));
            transcript.push(status("reply"));
        }
        // 21 turns exist; trim keeps the newest 15 (turns 6..=20).
        transcript.trim();
        let turns: Vec<usize> = transcript.entries.iter().map(|e| e.turn).collect();
        assert_eq!(
            *turns.first().unwrap(),
            HYSTERESIS + 1,
            "oldest dropped: {turns:?}"
        );
        assert_eq!(*turns.last().unwrap(), MAX_TURNS + HYSTERESIS);
    }

    #[test]
    fn trim_below_threshold_is_noop() {
        let mut transcript = Transcript::new();
        for turn in 0..MAX_TURNS + HYSTERESIS - 1 {
            transcript.push_new_turn(status(&format!("u{turn}")));
        }
        assert_eq!(transcript.trim(), 0);
    }

    #[test]
    fn render_concatenates_entries() {
        theme::set(theme::Theme::dark());
        let mut transcript = Transcript::new();
        transcript.push_new_turn(status("alpha"));
        transcript.push(status("beta"));
        let lines = transcript.render(60);
        let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(plain.iter().any(|l| l.contains("alpha")));
        assert!(plain.iter().any(|l| l.contains("beta")));
    }
}
