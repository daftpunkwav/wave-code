//! The multi-line input editor: the largest built-in component.
//!
//! Features: grapheme-correct cursor movement and deletion, CJK/emoji
//! aware wrapping, vertical scroll window with border indicators, input
//! history with draft preservation, kill ring + undo, bracketed-paste
//! collapse into atomic markers, slash-token highlighting, argument
//! ghost hints, and an autocomplete popup driven by a
//! [`CompletionProvider`].
//!
//! Submit contract: a bare Enter yields [`EditorAction::Submit`] with
//! the fully expanded text (paste markers expanded); Shift+Enter and
//! Ctrl+J insert newlines; a backslash before Enter is exchanged for a
//! newline (the legacy-terminal multiline workaround). Up/Down fall
//! through as [`EditorAction::Passthrough`] when the cursor sits on the
//! first/last visual row, so the host can bind history recall there.

use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::autocomplete::{
    AutocompletePopup, CompletionProvider, Trigger, TriggerKind, detect_trigger,
};
use crate::border;
use crate::color::Style;
use crate::component::Component;
use crate::keys::{Key, KeyEvent};
use crate::width::{self, Token};

/// Zero-width marker embedded at the cursor cell; the screen layer scans
/// for it, strips it, and positions the hardware cursor there (IME support).
pub const CURSOR_MARKER: &str = "\x1b_wc\x07";

/// Where the last yank inserted text: yank-pop replaces exactly that
/// span while cycling the ring.
#[derive(Debug, Clone, Copy)]
struct YankRecord {
    row: usize,
    /// Grapheme column the yanked text starts at.
    col: usize,
    ring_index: usize,
}

/// Editor visual style knobs, themed by the application layer.
#[derive(Debug, Clone)]
pub struct EditorStyle {
    /// Border color (switched to accent colors in plan/shell modes).
    pub border: Style,
    /// The prompt glyph (the keyed-in chevron `❯`, or `!` in
    /// shell mode).
    pub prompt: Style,
    /// Bold highlight for a leading `/command` token.
    pub slash_command: Style,
    /// Highlight for a leading `!command` token (shell mode).
    pub shell_command: Style,
    /// Dim ghost text for argument hints.
    pub hint: Style,
    /// Dim styling for collapsed paste markers.
    pub paste_marker: Style,
    /// Popup styles (selected/label/description).
    pub popup: crate::select_list::SelectListStyle,
}

impl Default for EditorStyle {
    fn default() -> Self {
        Self {
            border: Style::new(),
            prompt: Style::new(),
            slash_command: Style::new().bold(),
            shell_command: Style::new(),
            hint: Style::new().dim(),
            paste_marker: Style::new().dim(),
            popup: crate::select_list::SelectListStyle {
                selected: Style::new(),
                label: Style::new(),
                description: Style::new().dim(),
            },
        }
    }
}

/// What the editor wants the host to do after a key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAction {
    /// The event was consumed; nothing further.
    Handled,
    /// The event was not consumed (host-level binding).
    Passthrough,
    /// Submit the expanded buffer; the editor is cleared afterwards.
    Submit(String),
}

#[derive(Debug, Default)]
struct History {
    entries: Vec<String>,
    position: Option<usize>,
    draft: Option<String>,
}

/// A collapsed paste: buffer text replaced by an atomic marker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PasteMarker {
    id: usize,
    text: String,
    lines: usize,
}

/// One wrapped visual row of one logical line.
#[derive(Debug)]
struct VisualRow {
    text: String,
    line: usize,
    /// Grapheme index of the row start within the logical line.
    start: usize,
    /// Grapheme index one past the row end.
    end: usize,
}

/// The multi-line input editor.
pub struct Editor {
    lines: Vec<String>,
    row: usize,
    /// Cursor column as a grapheme index into `lines[row]`.
    col: usize,
    /// Visual column goal held during vertical movement.
    goal_col: Option<usize>,
    /// Body width of the last render; vertical movement wraps with the
    /// same budget so visual rows match what the user sees.
    last_body_width: usize,
    style: EditorStyle,
    prompt: String,
    label: Option<String>,
    argument_hint: Option<String>,
    history: History,
    undo: Vec<(Vec<String>, usize, usize)>,
    /// Killed text, newest first (capped); Ctrl+Y yanks `[0]`, Alt+Y
    /// cycles through the rest.
    kill_ring: Vec<String>,
    /// Cursor position where the current kill sequence last ended; a
    /// new kill merges into the same entry only when the cursor has
    /// not moved since (Emacs-style consecutive kills).
    last_kill_pos: Option<(usize, usize)>,
    /// Where the last yank landed, for Alt+Y yank-pop cycling.
    last_yank: Option<YankRecord>,
    paste_markers: Vec<PasteMarker>,
    next_paste_id: usize,
    provider: Option<Box<dyn CompletionProvider>>,
    popup: AutocompletePopup,
    popup_trigger: Option<Trigger>,
}

impl Editor {
    /// An empty editor with default styling.
    pub fn new(style: EditorStyle) -> Self {
        let mut popup = AutocompletePopup::new(EditorStyle::default().popup, 5);
        popup.set_max_visible(5);
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            goal_col: None,
            last_body_width: 40,
            style,
            prompt: "❯".to_string(),
            label: None,
            argument_hint: None,
            history: History::default(),
            undo: Vec::new(),
            kill_ring: Vec::new(),
            last_kill_pos: None,
            last_yank: None,
            paste_markers: Vec::new(),
            next_paste_id: 1,
            provider: None,
            popup,
            popup_trigger: None,
        }
    }

    /// Swap the styling (theme switches rebuild it).
    pub fn set_style(&mut self, style: EditorStyle) {
        self.style = style;
    }

    /// Replace the completion provider (slash commands, files, skills).
    pub fn set_provider(&mut self, provider: Box<dyn CompletionProvider>) {
        self.provider = Some(provider);
    }

    /// Set the prompt glyph (e.g. `!` for shell commands).
    pub fn set_prompt(&mut self, prompt: impl Into<String>) {
        self.prompt = prompt.into();
    }

    /// Restyle the frame border (permission-mode accent: plan →
    /// primary, auto → warning). Takes effect on the next render.
    pub fn set_border_style(&mut self, border: Style) {
        self.style.border = border;
    }

    /// Set the top-border label (e.g. ` ! shell mode `).
    pub fn set_label(&mut self, label: Option<String>) {
        self.label = label;
    }

    /// Set the dim ghost hint shown after the cursor (slash arguments).
    pub fn set_argument_hint(&mut self, hint: Option<String>) {
        self.argument_hint = hint;
    }

    /// Current buffer content joined with newlines (markers NOT expanded;
    /// expansion happens on submit).
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// True when the buffer holds no visible text.
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Replace the buffer content (history recall, queued-message edit).
    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(|s| s.to_string()).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.graphemes(self.row).len();
        self.goal_col = None;
        self.refresh_popup();
    }

    /// Clear the buffer (Ctrl+C cascade).
    pub fn clear(&mut self) {
        self.set_text("");
        self.history.draft = None;
    }

    /// Record an entry into input history: dedupes against the most
    /// recent entry and caps at 100.
    pub fn remember_history(&mut self, entry: &str) {
        if self
            .history
            .entries
            .first()
            .map(|e| e == entry)
            .unwrap_or(false)
        {
            return;
        }
        self.history.entries.insert(0, entry.to_string());
        self.history.entries.truncate(100);
        self.history.position = None;
        self.history.draft = None;
    }

    /// Seed browsing history from persisted entries (oldest first).
    pub fn load_history(&mut self, entries: Vec<String>) {
        let mut entries = entries;
        entries.reverse(); // browse newest-first
        entries.truncate(100);
        self.history.entries = entries;
    }

    /// Snapshot all history entries, oldest first (persistence).
    pub fn history_entries(&self) -> Vec<String> {
        let mut entries = self.history.entries.clone();
        entries.reverse();
        entries
    }

    // ----- helpers ---------------------------------------------------------

    fn graphemes(&self, row: usize) -> Vec<&str> {
        self.lines
            .get(row)
            .map(|l| l.graphemes(true).collect())
            .unwrap_or_default()
    }

    fn undo_push(&mut self) {
        // Any non-kill edit makes a pending yank-pop span stale.
        self.last_yank = None;
        self.undo.push((self.lines.clone(), self.row, self.col));
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
    }

    fn undo_pop(&mut self) {
        if let Some((lines, row, col)) = self.undo.pop() {
            self.lines = lines;
            self.row = row.min(self.lines.len() - 1);
            self.col = col.min(self.graphemes(self.row).len());
        }
    }

    // ----- text mutation ---------------------------------------------------

    /// Insert text at the cursor; `\n` splits into new logical lines.
    pub fn insert_text(&mut self, text: &str) {
        self.undo_push();
        let mut fragments = text.split('\n');
        if let Some(first) = fragments.next() {
            self.insert_fragment(first);
        }
        for fragment in fragments {
            self.split_at_cursor();
            self.insert_fragment(fragment);
        }
        self.goal_col = None;
        self.refresh_popup();
    }

    fn insert_fragment(&mut self, fragment: &str) {
        let line = &mut self.lines[self.row];
        let byte = grapheme_byte_offset(line, self.col);
        line.insert_str(byte, fragment);
        self.col += fragment.graphemes(true).count();
    }

    fn split_at_cursor(&mut self) {
        let line = self.lines[self.row].clone();
        let byte = grapheme_byte_offset(&line, self.col);
        let (head, tail) = line.split_at(byte);
        self.lines[self.row] = head.to_string();
        self.lines.insert(self.row + 1, tail.to_string());
        self.row += 1;
        self.col = 0;
    }

    fn join_with_next(&mut self) {
        if self.row + 1 >= self.lines.len() {
            return;
        }
        let next = self.lines.remove(self.row + 1);
        self.col = self.graphemes(self.row).len();
        self.lines[self.row].push_str(&next);
    }

    /// Insert a paste; large pastes collapse into an atomic marker.
    pub fn insert_paste(&mut self, text: &str) {
        let line_count = text.split('\n').count();
        let char_count = text.chars().count();
        if line_count > 10 || char_count > 1000 {
            self.undo_push();
            let id = self.next_paste_id;
            self.next_paste_id += 1;
            let marker = if line_count > 10 {
                format!("[paste #{id} +{} lines]", line_count - 1)
            } else {
                format!("[paste #{id} {char_count} chars]")
            };
            self.paste_markers.push(PasteMarker {
                id,
                text: text.to_string(),
                lines: line_count,
            });
            self.insert_text(&marker);
        } else {
            self.insert_text(text);
        }
    }

    /// Expand all paste markers in `text` (the submit path).
    pub fn expand_markers(&self, text: &str) -> String {
        let mut out = text.to_string();
        for marker in &self.paste_markers {
            let short = format!("[paste #{} +{} lines]", marker.id, marker.lines - 1);
            let wide = format!(
                "[paste #{} {} chars]",
                marker.id,
                marker.text.chars().count()
            );
            if out.contains(&short) {
                out = out.replace(&short, &marker.text);
            } else if out.contains(&wide) {
                out = out.replace(&wide, &marker.text);
            }
        }
        out
    }

    fn clear_markers(&mut self) {
        self.paste_markers.clear();
    }

    /// Backspace: one grapheme, a whole paste marker, or a line join.
    fn backspace(&mut self) {
        if self.col > 0 {
            if let Some(marker) = self.marker_before_cursor() {
                self.undo_push();
                let line = &mut self.lines[self.row];
                let byte_end = grapheme_byte_offset(line, self.col);
                let byte_start = byte_end - marker.len();
                line.replace_range(byte_start..byte_end, "");
                self.col -= marker.graphemes(true).count();
                return;
            }
            self.undo_push();
            let line = &mut self.lines[self.row];
            let byte = grapheme_byte_offset(line, self.col);
            let prev = grapheme_byte_offset(line, self.col - 1);
            line.replace_range(prev..byte, "");
            self.col -= 1;
            return;
        }
        if self.row > 0 {
            self.undo_push();
            let above = self.lines.remove(self.row - 1);
            self.row -= 1;
            let current = self.lines.remove(self.row);
            self.lines.insert(self.row, format!("{above}{current}"));
            self.col = above.graphemes(true).count();
        }
    }

    /// The marker string ending exactly at the cursor, if any.
    fn marker_before_cursor(&self) -> Option<String> {
        let line = &self.lines[self.row];
        let byte_end = grapheme_byte_offset(line, self.col);
        let head = &line[..byte_end];
        for marker in &self.paste_markers {
            let short = format!("[paste #{} +{} lines]", marker.id, marker.lines - 1);
            let wide = format!(
                "[paste #{} {} chars]",
                marker.id,
                marker.text.chars().count()
            );
            if head.ends_with(&short) {
                return Some(short);
            }
            if head.ends_with(&wide) {
                return Some(wide);
            }
        }
        None
    }

    fn delete_forward(&mut self) {
        let count = self.graphemes(self.row).len();
        if self.col < count {
            self.undo_push();
            let line = &mut self.lines[self.row];
            let byte_start = grapheme_byte_offset(line, self.col);
            let byte_end = grapheme_byte_offset(line, self.col + 1);
            line.replace_range(byte_start..byte_end, "");
        } else if self.row + 1 < self.lines.len() {
            self.undo_push();
            self.join_with_next();
        }
    }

    /// Push killed text onto the ring. A kill starting exactly where
    /// the previous kill ended merges into the same entry (Emacs-style
    /// `C-u C-u` accumulates); `at` is the pre-kill cursor position.
    fn kill_push(&mut self, text: String, forward: bool, at: (usize, usize)) {
        if !text.is_empty() {
            let merge = self.last_kill_pos == Some(at) && !self.kill_ring.is_empty();
            if merge {
                let front = &mut self.kill_ring[0];
                if forward {
                    front.push_str(&text);
                } else {
                    front.insert_str(0, &text);
                }
            } else {
                self.kill_ring.insert(0, text);
                self.kill_ring.truncate(32);
            }
        }
        self.last_kill_pos = Some((self.row, self.col));
    }

    fn kill_to_line_end(&mut self) {
        let count = self.graphemes(self.row).len();
        if self.col >= count {
            return;
        }
        self.undo_push();
        let at = (self.row, self.col);
        let line = self.lines[self.row].clone();
        let start = grapheme_byte_offset(&line, self.col);
        let killed = line[start..].to_string();
        self.lines[self.row].truncate(start);
        self.kill_push(killed, true, at);
        self.refresh_popup();
    }

    fn kill_to_line_start(&mut self) {
        if self.col == 0 {
            return;
        }
        self.undo_push();
        let at = (self.row, self.col);
        let line = self.lines[self.row].clone();
        let end = grapheme_byte_offset(&line, self.col);
        let killed = line[..end].to_string();
        self.lines[self.row] = line[end..].to_string();
        self.col = 0;
        self.kill_push(killed, false, at);
        self.refresh_popup();
    }

    fn kill_word_back(&mut self) {
        if self.col == 0 {
            self.backspace();
            return;
        }
        self.undo_push();
        let at = (self.row, self.col);
        let line = self.lines[self.row].clone();
        let graphemes: Vec<&str> = line.graphemes(true).collect();
        let mut end = self.col;
        while end > 0 && graphemes[end - 1] == " " {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && graphemes[start - 1] != " " {
            start -= 1;
        }
        let byte_start = grapheme_byte_offset(&line, start);
        let byte_end = grapheme_byte_offset(&line, self.col);
        let killed = line[byte_start..byte_end].to_string();
        self.lines[self.row].replace_range(byte_start..byte_end, "");
        self.col = start;
        self.kill_push(killed, false, at);
        self.refresh_popup();
    }

    fn yank(&mut self) {
        let Some(text) = self.kill_ring.first().filter(|t| !t.is_empty()).cloned() else {
            return;
        };
        let col = self.col;
        let row = self.row;
        self.insert_text(&text);
        self.last_yank = Some(YankRecord {
            row,
            col,
            ring_index: 0,
        });
    }

    /// Alt+Y right after a yank: replace the just-yanked span with the
    /// next ring entry, cycling. Anything that invalidates the recorded
    /// span (a move across lines, another edit) disarms it.
    fn yank_pop(&mut self) {
        let Some(record) = self.last_yank else {
            return;
        };
        let Some(current) = self.kill_ring.get(record.ring_index).cloned() else {
            self.last_yank = None;
            return;
        };
        let valid = self.row == record.row && self.col >= record.col && {
            let line = &self.lines[self.row];
            let byte_start = grapheme_byte_offset(line, record.col);
            let byte_end = grapheme_byte_offset(line, self.col);
            line.get(byte_start..byte_end) == Some(current.as_str())
        };
        if !valid {
            self.last_yank = None;
            return;
        }
        let next_index = (record.ring_index + 1) % self.kill_ring.len();
        let next = self.kill_ring[next_index].clone();
        self.undo_push();
        let line = &mut self.lines[self.row];
        let byte_start = grapheme_byte_offset(line, record.col);
        let byte_end = grapheme_byte_offset(line, self.col);
        line.replace_range(byte_start..byte_end, &next);
        self.col = record.col + next.graphemes(true).count();
        self.last_yank = Some(YankRecord {
            row: self.row,
            col: record.col,
            ring_index: next_index,
        });
        self.refresh_popup();
    }

    // ----- cursor movement ---------------------------------------------------

    fn move_left(&mut self) {
        self.goal_col = None;
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.graphemes(self.row).len();
        }
    }

    fn move_right(&mut self) {
        self.goal_col = None;
        let count = self.graphemes(self.row).len();
        if self.col < count {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Move up one visual row. Returns false when already on the first
    /// visual row (the host binds history recall there).
    pub fn move_up(&mut self) -> bool {
        let rows = self.build_visual(self.current_body_width());
        let current = self.cursor_row_index(&rows);
        if current == 0 {
            return false;
        }
        self.move_to_visual_row(&rows, current - 1);
        true
    }

    /// Move down one visual row. Returns false when already on the last.
    pub fn move_down(&mut self) -> bool {
        let rows = self.build_visual(self.current_body_width());
        let current = self.cursor_row_index(&rows);
        if current + 1 >= rows.len() {
            return false;
        }
        self.move_to_visual_row(&rows, current + 1);
        true
    }

    fn move_to_visual_row(&mut self, rows: &[VisualRow], target: usize) {
        let goal = self
            .goal_col
            .unwrap_or_else(|| self.cursor_visual_col(rows));
        self.goal_col = Some(goal);
        let row = &rows[target];
        self.row = row.line;
        self.col = col_at_visual_col(&self.lines[row.line], row.start, goal);
    }

    /// Render-time body width; vertical movement uses the same wrap as
    /// the last frame so visual rows match what the user sees.
    fn current_body_width(&self) -> usize {
        self.last_body_width.max(1)
    }

    // ----- autocomplete --------------------------------------------------------

    /// Recompute popup state from the buffer (call after any mutation).
    fn refresh_popup(&mut self) {
        let Some(provider) = self.provider.as_deref() else {
            self.popup_trigger = None;
            return;
        };
        let first_line = self.lines.first().map(String::as_str).unwrap_or("");
        let slash = if self.row == 0 {
            // Slash commands are a line-0 construct; detect against the
            // real cursor so the replacement range stays consistent.
            let cursor_byte = grapheme_byte_offset(first_line, self.col);
            detect_trigger(first_line, cursor_byte, TriggerKind::Slash)
        } else {
            None
        };
        let cursor_line = &self.lines[self.row];
        let cursor_byte = grapheme_byte_offset(cursor_line, self.col);
        let mention = detect_trigger(cursor_line, cursor_byte, TriggerKind::Mention);
        let trigger = slash.or(mention);
        self.popup.update(provider, trigger.clone());
        self.popup_trigger = if self.popup.is_empty() { None } else { trigger };
    }

    /// True when the autocomplete popup is visible.
    pub fn popup_open(&self) -> bool {
        self.popup_trigger.is_some()
    }

    fn accept_completion(&mut self) {
        let Some(trigger) = self.popup_trigger.clone() else {
            return;
        };
        // A stale trigger (cursor moved without a refresh) must never
        // produce an inverted replacement range: drop the popup instead.
        if self.col < trigger_start_col(&self.lines[self.row], trigger.start) {
            self.close_popup();
            return;
        }
        let Some(completion) = self.popup.selected_completion().cloned() else {
            return;
        };
        let line = &mut self.lines[self.row];
        let token_end = grapheme_byte_offset(line, self.col);
        line.replace_range(trigger.start..token_end, &completion.insert);
        self.col = line[..trigger.start + completion.insert.len()]
            .graphemes(true)
            .count();
        self.refresh_popup();
    }

    fn close_popup(&mut self) {
        self.popup_trigger = None;
    }

    // ----- key handling -----------------------------------------------------------

    /// Handle one key event; see [`EditorAction`] for the contract.
    pub fn handle_key(&mut self, event: KeyEvent) -> EditorAction {
        if self.popup_open() {
            match event.key {
                Key::Up => {
                    self.popup.list_mut().select_previous();
                    return EditorAction::Handled;
                }
                Key::Down => {
                    self.popup.list_mut().select_next();
                    return EditorAction::Handled;
                }
                Key::Esc => {
                    self.close_popup();
                    return EditorAction::Handled;
                }
                Key::Tab => {
                    self.accept_completion();
                    return EditorAction::Handled;
                }
                Key::Enter if !event.mods.shift && !event.mods.ctrl => {
                    self.accept_completion();
                    return self.submit();
                }
                _ => {}
            }
        }
        match (event.key, event.mods) {
            (Key::Enter, m) if m.ctrl || m.shift => {
                self.undo_push();
                self.split_at_cursor();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Enter, _) => {
                // Backslash-enter: exchange a trailing `\` for a newline.
                let before =
                    self.col > 0 && self.graphemes(self.row).get(self.col - 1) == Some(&"\\");
                if before {
                    self.undo_push();
                    let line = &mut self.lines[self.row];
                    let byte = grapheme_byte_offset(line, self.col - 1);
                    line.replace_range(byte..byte + 1, "");
                    self.col -= 1;
                    self.split_at_cursor();
                    self.refresh_popup();
                    return EditorAction::Handled;
                }
                self.submit()
            }
            (Key::Tab, _) => EditorAction::Handled,
            (Key::Backspace, m) if m.alt => {
                self.kill_word_back();
                EditorAction::Handled
            }
            (Key::Backspace, _) => {
                self.backspace();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Delete, _) => {
                self.delete_forward();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Left, m) if m.ctrl || m.alt => {
                self.goal_col = None;
                self.move_word_left();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Right, m) if m.ctrl || m.alt => {
                self.goal_col = None;
                self.move_word_right();
                self.refresh_popup();
                EditorAction::Handled
            }
            // Emacs-style word jumps: Alt+B back, Alt+F forward.
            (Key::Char('b'), m) if m.alt && !m.ctrl => {
                self.goal_col = None;
                self.move_word_left();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Char('f'), m) if m.alt && !m.ctrl => {
                self.goal_col = None;
                self.move_word_right();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Left, _) => {
                self.move_left();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Right, _) => {
                self.move_right();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Up, _) => {
                if self.move_up() {
                    self.refresh_popup();
                    EditorAction::Handled
                } else {
                    self.goal_col = None;
                    EditorAction::Passthrough
                }
            }
            (Key::Down, _) => {
                if self.move_down() {
                    self.refresh_popup();
                    EditorAction::Handled
                } else {
                    self.goal_col = None;
                    EditorAction::Passthrough
                }
            }
            (Key::Home, _) => {
                self.goal_col = None;
                self.col = 0;
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Char('a'), m) if m.ctrl => {
                self.goal_col = None;
                self.col = 0;
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::End, _) => {
                self.goal_col = None;
                self.col = self.graphemes(self.row).len();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Char('e'), m) if m.ctrl => {
                self.goal_col = None;
                self.col = self.graphemes(self.row).len();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Char('u'), m) if m.ctrl => {
                self.kill_to_line_start();
                EditorAction::Handled
            }
            (Key::Char('k'), m) if m.ctrl => {
                self.kill_to_line_end();
                EditorAction::Handled
            }
            (Key::Char('w'), m) if m.ctrl => {
                self.kill_word_back();
                EditorAction::Handled
            }
            (Key::Char('y'), m) if m.ctrl => {
                self.yank();
                EditorAction::Handled
            }
            (Key::Char('y'), m) if m.alt && !m.ctrl => {
                self.yank_pop();
                EditorAction::Handled
            }
            (Key::Char('-'), m) if m.ctrl => {
                self.undo_pop();
                self.refresh_popup();
                EditorAction::Handled
            }
            (Key::Char(c), m) if !m.ctrl && !m.alt => {
                self.insert_text(&c.to_string());
                EditorAction::Handled
            }
            _ => EditorAction::Passthrough,
        }
    }

    fn submit(&mut self) -> EditorAction {
        if self.is_empty() {
            return EditorAction::Passthrough;
        }
        let raw = self.text();
        let expanded = self.expand_markers(&raw);
        // History stores the expanded text: recalling a pasted entry
        // must resubmit the paste body, never the collapsed marker.
        self.remember_history(&expanded);
        self.set_text("");
        self.clear_markers();
        EditorAction::Submit(expanded)
    }

    /// History recall on Up: load the previous entry, saving the draft.
    pub fn history_previous(&mut self) -> bool {
        if self.history.entries.is_empty() {
            return false;
        }
        let next = match self.history.position {
            Some(position) => (position + 1).min(self.history.entries.len() - 1),
            None => {
                self.history.draft = Some(self.text());
                0
            }
        };
        self.history.position = Some(next);
        let entry = self.history.entries[next].clone();
        self.set_text(&entry);
        true
    }

    /// History recall on Down: next entry, or restore the draft at the end.
    pub fn history_next(&mut self) -> bool {
        match self.history.position {
            None => false,
            Some(0) => {
                if let Some(draft) = self.history.draft.take() {
                    self.history.position = None;
                    self.set_text(&draft);
                    true
                } else {
                    false
                }
            }
            Some(position) => {
                self.history.position = Some(position - 1);
                let entry = self.history.entries[position - 1].clone();
                self.set_text(&entry);
                true
            }
        }
    }

    fn move_word_left(&mut self) {
        if self.col == 0 {
            self.move_left();
            return;
        }
        let graphemes = self.graphemes(self.row);
        let mut target = self.col;
        while target > 0 && graphemes[target - 1] == " " {
            target -= 1;
        }
        while target > 0 && graphemes[target - 1] != " " {
            target -= 1;
        }
        self.col = target;
    }

    fn move_word_right(&mut self) {
        let count = self.graphemes(self.row).len();
        if self.col >= count {
            self.move_right();
            return;
        }
        let graphemes = self.graphemes(self.row);
        let mut target = self.col;
        while target < count && graphemes[target] == " " {
            target += 1;
        }
        while target < count && graphemes[target] != " " {
            target += 1;
        }
        self.col = target;
    }

    // ----- rendering -----------------------------------------------------------

    /// Render the framed editor at `width`, using `terminal_rows` for the
    /// vertical size budget.
    pub fn render_box(&mut self, total_width: usize, terminal_rows: usize) -> Vec<String> {
        let inner_width = total_width.saturating_sub(2);
        if inner_width < 10 {
            return self.lines.clone();
        }
        let content_width = inner_width - 2; // flanking spaces around the body
        let prompt_span = width::width(&self.prompt) + 1;
        let text_budget = content_width.saturating_sub(prompt_span).max(1);
        self.last_body_width = text_budget.max(1);

        let rows = self.build_visual(text_budget);
        let empty = self.is_empty();
        let cap = if empty {
            5.max(terminal_rows * 3 / 10)
        } else {
            (terminal_rows * 5 / 10).max(8)
        };
        let cursor_index = self.cursor_row_index(&rows);
        let cursor_col = self.cursor_visual_col(&rows);

        let visible = rows.len().min(cap);
        let start = if cursor_index >= visible {
            (cursor_index + 1 - visible).min(rows.len() - visible)
        } else {
            0
        };
        let end = (start + visible).min(rows.len());
        let hidden_above = start;
        let hidden_below = rows.len() - end;

        let prompt_width = width::width(&self.prompt);
        let mut content: Vec<String> = Vec::with_capacity(visible);
        for (offset, row) in rows[start..end].iter().enumerate() {
            let is_cursor_row = start + offset == cursor_index;
            let prefix = if row.start == 0 {
                format!("{} ", self.style.prompt.paint(&self.prompt))
            } else {
                format!("{} ", " ".repeat(prompt_width))
            };
            let mut text = row.text.clone();
            if is_cursor_row {
                // `cursor_col` is already the cursor's visible column
                // within this row.
                text = insert_cursor_marker(&text, cursor_col);
            }
            // Leading `/command` and `!command` tokens are painted on
            // the very first row only.
            if row.line == 0 && row.start == 0 {
                text = paint_leading_token(&text, &self.style);
            }
            let body = format!("{prefix}{text}");
            let mut padded = width::pad_to_width(&body, content_width);
            if width::width(&padded) > content_width {
                padded = width::truncate_to_width(&padded, content_width);
            }
            content.push(padded);
        }
        if hidden_above > 0 && !content.is_empty() {
            content[0] = border::scroll_label(inner_width, "↑", hidden_above);
        }
        if hidden_below > 0 && !content.is_empty() {
            let last = content.len() - 1;
            content[last] = border::scroll_label(inner_width, "↓", hidden_below);
        }

        let mut out = Vec::with_capacity(content.len() + 2);
        let top_mid = match &self.label {
            Some(label) => {
                let label = width::truncate_to_width(label, inner_width.saturating_sub(4));
                let pad = inner_width.saturating_sub(2 + width::width(&label));
                format!("─ {label}{}", "─".repeat(pad))
            }
            None => "─".repeat(inner_width),
        };
        let border_style = self.style.border;
        out.push(border_style.paint(format!("╭{top_mid}╮").as_str()));
        for line in content {
            let mut row = String::with_capacity(total_width + 16);
            row.push_str(&border_style.paint("│"));
            row.push(' ');
            row.push_str(&line);
            row.push(' ');
            row.push_str(&border_style.paint("│"));
            out.push(row);
        }
        out.push(border_style.paint(format!("╰{}╯", "─".repeat(inner_width)).as_str()));
        self.render_popup_row(&mut out, total_width);
        out
    }

    /// Append the autocomplete popup rows under the box, when open.
    fn render_popup_row(&mut self, out: &mut Vec<String>, total_width: usize) {
        if !self.popup_open() {
            return;
        }
        // The popup re-renders per frame and the rows are transformed
        // (indented) here, so the shared array is consumed by value.
        let popup_rows =
            Arc::try_unwrap(self.popup.list_mut().render(total_width.saturating_sub(2)))
                .unwrap_or_else(|rows| rows.as_ref().clone());
        for row in &popup_rows {
            out.push(format!("  {row}"));
        }
    }
}

/// Paint the first whitespace-delimited token of the input's first row:
/// `/command` in the slash style, `!command` in the shell style. Any
/// other leading character returns the row untouched.
fn paint_leading_token(text: &str, style: &EditorStyle) -> String {
    let token_end = text.find(char::is_whitespace).unwrap_or(text.len());
    let (token, rest) = text.split_at(token_end);
    let painted = if token.starts_with('/') {
        style.slash_command.paint(token)
    } else if token.starts_with('!') {
        style.shell_command.paint(token)
    } else {
        return text.to_string();
    };
    format!("{painted}{rest}")
}

/// Byte offset of grapheme index `col` in `line`.
fn grapheme_byte_offset(line: &str, col: usize) -> usize {
    line.grapheme_indices(true)
        .nth(col)
        .map(|(byte, _)| byte)
        .unwrap_or(line.len())
}

/// Grapheme index of a byte offset in `line` (trigger range guards).
fn trigger_start_col(line: &str, byte: usize) -> usize {
    line[..byte].graphemes(true).count()
}

/// Grapheme index for the visual column `goal`, starting the search at
/// grapheme index `from` (a visual row's start).
fn col_at_visual_col(line: &str, from: usize, goal: usize) -> usize {
    let mut used = 0usize;
    for (offset, g) in line.graphemes(true).skip(from).enumerate() {
        if used >= goal {
            return from + offset;
        }
        used += g.width();
    }
    line.graphemes(true).count()
}

impl Editor {
    /// Word-aware wrap of every logical line into visual rows.
    fn build_visual(&self, text_budget: usize) -> Vec<VisualRow> {
        let mut out = Vec::new();
        for (line_index, line) in self.lines.iter().enumerate() {
            let graphemes: Vec<&str> = line.graphemes(true).collect();
            if graphemes.is_empty() {
                out.push(VisualRow {
                    text: String::new(),
                    line: line_index,
                    start: 0,
                    end: 0,
                });
                continue;
            }
            let mut current = String::new();
            let mut used = 0usize;
            let mut row_start = 0usize;
            for (index, g) in graphemes.iter().enumerate() {
                let w = g.width();
                if used + w > text_budget && used > 0 {
                    out.push(VisualRow {
                        text: std::mem::take(&mut current),
                        line: line_index,
                        start: row_start,
                        end: index,
                    });
                    used = 0;
                    row_start = index;
                    if *g == " " {
                        row_start = index + 1;
                        continue;
                    }
                }
                current.push_str(g);
                used += w;
            }
            if !current.is_empty() || row_start == graphemes.len() {
                out.push(VisualRow {
                    text: current,
                    line: line_index,
                    start: row_start,
                    end: graphemes.len(),
                });
            }
        }
        out
    }

    /// Visual row index containing the cursor.
    fn cursor_row_index(&self, rows: &[VisualRow]) -> usize {
        for (index, row) in rows.iter().enumerate() {
            if row.line == self.row && row.start <= self.col && self.col < row.end {
                return index;
            }
        }
        // Cursor at line end (or empty line): the line's last row.
        rows.iter()
            .rposition(|row| row.line == self.row)
            .unwrap_or(0)
    }

    /// Visible column of the cursor within its visual row.
    fn cursor_visual_col(&self, rows: &[VisualRow]) -> usize {
        let index = self.cursor_row_index(rows);
        let row = &rows[index];
        self.graphemes(row.line)
            .iter()
            .take(self.col.saturating_sub(row.start))
            .map(|g| g.width())
            .sum()
    }
}

/// Insert the cursor marker into a styled row at visible column `col`.
fn insert_cursor_marker(row: &str, col: usize) -> String {
    let mut out = String::with_capacity(row.len() + CURSOR_MARKER.len());
    let mut used = 0usize;
    let mut inserted = false;
    for token in width::tokens(row) {
        match token {
            Token::Escape(seq) => out.push_str(seq),
            Token::Grapheme(g) => {
                if !inserted && used >= col {
                    out.push_str(CURSOR_MARKER);
                    inserted = true;
                }
                out.push_str(g);
                used += g.width();
            }
        }
    }
    if !inserted {
        out.push_str(CURSOR_MARKER);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::Mods;
    use crate::width::strip_ansi;

    fn editor() -> Editor {
        Editor::new(EditorStyle::default())
    }

    fn type_string(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            let event = KeyEvent::plain(Key::Char(c));
            let _ = editor.handle_key(event);
        }
    }

    #[test]
    fn typing_and_submit_roundtrip() {
        let mut editor = editor();
        type_string(&mut editor, "hello");
        assert_eq!(editor.text(), "hello");
        let action = editor.handle_key(KeyEvent::plain(Key::Enter));
        assert_eq!(action, EditorAction::Submit("hello".to_string()));
        assert!(editor.is_empty());
    }

    /// A style painted only via one distinctive SGR attribute so tests
    /// can assert which span it wrapped.
    fn marked_style() -> EditorStyle {
        EditorStyle {
            slash_command: Style::new().bold(),
            shell_command: Style::new().underline(),
            ..EditorStyle::default()
        }
    }

    /// Plain-text view of one editor body row (borders kept, cursor
    /// marker stripped — the screen layer removes it at draw time).
    fn plain_row(rows: &[String], index: usize) -> String {
        strip_ansi(&rows[index]).replace(CURSOR_MARKER, "")
    }

    #[test]
    fn leading_slash_token_is_painted() {
        let mut editor = Editor::new(marked_style());
        type_string(&mut editor, "/help now");
        let rows = editor.render_box(40, 4);
        let body = &rows[1];
        assert!(body.contains("\x1b[1m"), "bold slash token: {body:?}");
        assert!(!body.contains("\x1b[4m"), "no shell style: {body:?}");
        assert!(plain_row(&rows, 1).contains("❯ /help now"), "{body:?}");
    }

    #[test]
    fn leading_shell_token_is_painted() {
        let mut editor = Editor::new(marked_style());
        type_string(&mut editor, "!ls -la");
        let rows = editor.render_box(40, 4);
        let body = &rows[1];
        assert!(body.contains("\x1b[4m"), "underlined shell token: {body:?}");
        assert!(!body.contains("\x1b[1m"), "no slash style: {body:?}");
        assert!(plain_row(&rows, 1).contains("❯ !ls -la"), "{body:?}");
    }

    #[test]
    fn plain_text_has_no_token_paint() {
        let mut editor = Editor::new(marked_style());
        type_string(&mut editor, "hello world");
        let rows = editor.render_box(40, 4);
        let body = &rows[1];
        assert!(
            !body.contains("\x1b[1m") && !body.contains("\x1b[4m"),
            "unpainted body: {body:?}"
        );
    }

    #[test]
    fn token_paint_stays_on_first_row() {
        let mut editor = Editor::new(marked_style());
        type_string(&mut editor, "/cmd");
        let _ = editor.handle_key(KeyEvent::new(
            Key::Enter,
            Mods {
                shift: true,
                ..Mods::NONE
            },
        ));
        type_string(&mut editor, "more");
        let rows = editor.render_box(40, 6);
        let body = &rows[2];
        assert!(
            !body.contains("\x1b[1m"),
            "continuation rows stay unpainted: {body:?}"
        );
        assert!(plain_row(&rows, 2).contains("more"), "{body:?}");
    }

    #[test]
    fn empty_enter_passes_through() {
        let mut editor = editor();
        assert_eq!(
            editor.handle_key(KeyEvent::plain(Key::Enter)),
            EditorAction::Passthrough
        );
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut editor = editor();
        type_string(&mut editor, "ab");
        let _ = editor.handle_key(KeyEvent::new(
            Key::Enter,
            Mods {
                shift: true,
                ..Mods::NONE
            },
        ));
        type_string(&mut editor, "cd");
        assert_eq!(editor.text(), "ab\ncd");
    }

    #[test]
    fn backslash_enter_exchanges_for_newline() {
        let mut editor = editor();
        type_string(&mut editor, "ab\\");
        let action = editor.handle_key(KeyEvent::plain(Key::Enter));
        assert_eq!(action, EditorAction::Handled);
        type_string(&mut editor, "cd");
        assert_eq!(editor.text(), "abcd".replace("abcd", "ab\ncd"));
    }

    #[test]
    fn backspace_joins_lines_and_deletes() {
        let mut editor = editor();
        editor.insert_text("ab\n");
        editor.insert_text("cd");
        assert_eq!(editor.text(), "ab\ncd");
        editor.handle_key(KeyEvent::plain(Key::Backspace));
        assert_eq!(editor.text(), "ab\nc");
        editor.handle_key(KeyEvent::plain(Key::Backspace));
        assert_eq!(editor.text(), "ab\n");
        editor.handle_key(KeyEvent::plain(Key::Backspace));
        assert_eq!(editor.text(), "ab", "join with previous line");
    }

    #[test]
    fn undo_restores_buffer() {
        let mut editor = editor();
        type_string(&mut editor, "hello");
        editor.handle_key(KeyEvent::new(Key::Char('-'), Mods::CTRL));
        assert_eq!(editor.text(), "hell");
    }

    #[test]
    fn kill_and_yank_roundtrip() {
        let mut editor = editor();
        type_string(&mut editor, "hello world");
        editor.handle_key(KeyEvent::new(Key::Char('a'), Mods::CTRL));
        editor.handle_key(KeyEvent::new(Key::Char('k'), Mods::CTRL));
        assert_eq!(editor.text(), "");
        editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
        assert_eq!(editor.text(), "hello world");
    }

    #[test]
    fn ctrl_w_kills_word_back() {
        let mut editor = editor();
        type_string(&mut editor, "hello world ");
        editor.handle_key(KeyEvent::new(Key::Char('w'), Mods::CTRL));
        assert_eq!(editor.text(), "hello ");
    }

    #[test]
    fn consecutive_same_position_kills_merge() {
        let mut editor = editor();
        type_string(&mut editor, "one two three");
        editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
        assert_eq!(editor.col, 8, "start of the last word");
        // Two kills starting at the same cursor position accumulate
        // into one yankable entry.
        editor.handle_key(KeyEvent::new(Key::Char('k'), Mods::CTRL));
        assert_eq!(editor.text(), "one two ");
        editor.handle_key(KeyEvent::new(Key::Char('w'), Mods::CTRL));
        assert_eq!(editor.text(), "one ");
        editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
        assert_eq!(editor.text(), "one two three", "merged kill yanks as one");
    }

    #[test]
    fn kills_from_different_positions_split_entries() {
        let mut editor = editor();
        type_string(&mut editor, "alpha beta");
        editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
        editor.handle_key(KeyEvent::new(Key::Char('u'), Mods::CTRL));
        assert_eq!(editor.text(), "beta");
        // A kill from a different position starts a new ring entry: the
        // next yank returns the newest kill, Alt+Y cycles back.
        editor.handle_key(KeyEvent::new(Key::Char('e'), Mods::CTRL));
        editor.handle_key(KeyEvent::new(Key::Char('w'), Mods::CTRL));
        assert_eq!(editor.text(), "");
        editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
        assert_eq!(editor.text(), "beta");
        editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::ALT));
        assert_eq!(editor.text(), "alpha ");
    }

    #[test]
    fn alt_y_cycles_through_the_ring() {
        let mut editor = editor();
        type_string(&mut editor, "alpha beta");
        // Kill "alpha " backward, then part of "beta" forward: two
        // entries (the cursor must sit before the line end for the
        // forward kill to take text).
        editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
        editor.handle_key(KeyEvent::new(Key::Char('u'), Mods::CTRL));
        assert_eq!(editor.text(), "beta");
        editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
        editor.handle_key(KeyEvent::new(Key::Right, Mods::NONE));
        editor.handle_key(KeyEvent::new(Key::Right, Mods::NONE));
        editor.handle_key(KeyEvent::new(Key::Char('k'), Mods::CTRL));
        assert_eq!(editor.text(), "be");
        // Clear the remaining draft (not a kill: the ring keeps its
        // three entries).
        editor.handle_key(KeyEvent::new(Key::Char('e'), Mods::CTRL));
        editor.handle_key(KeyEvent::plain(Key::Backspace));
        editor.handle_key(KeyEvent::plain(Key::Backspace));
        assert_eq!(editor.text(), "");
        // Yank inserts the newest kill; Alt+Y cycles both entries and
        // wraps.
        editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
        assert_eq!(editor.text(), "ta");
        editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::ALT));
        assert_eq!(editor.text(), "alpha ");
        editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::ALT));
        assert_eq!(editor.text(), "ta");
    }

    #[test]
    fn edit_between_yank_and_alt_y_disarms_the_cycle() {
        let mut editor = editor();
        type_string(&mut editor, "ab");
        editor.handle_key(KeyEvent::new(Key::Char('a'), Mods::CTRL));
        editor.handle_key(KeyEvent::new(Key::Char('k'), Mods::CTRL));
        editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::CTRL));
        assert_eq!(editor.text(), "ab");
        // Typing after the yank invalidates the recorded span.
        editor.handle_key(KeyEvent::plain(Key::Char('X')));
        editor.handle_key(KeyEvent::new(Key::Char('y'), Mods::ALT));
        assert_eq!(editor.text(), "abX", "stale yank-pop must not rewrite");
    }

    #[test]
    fn alt_b_and_alt_f_jump_words() {
        let mut editor = editor();
        type_string(&mut editor, "hello world");
        // Alt+F twice lands after "hello world"; Alt+B once lands
        // between the words.
        editor.handle_key(KeyEvent::new(Key::Char('f'), Mods::ALT));
        editor.handle_key(KeyEvent::new(Key::Char('f'), Mods::ALT));
        assert_eq!(editor.col, 11, "after the last word");
        editor.handle_key(KeyEvent::new(Key::Char('b'), Mods::ALT));
        assert_eq!(editor.col, 6, "start of the last word");
        // Insertion at the jumped position proves the cursor moved.
        editor.handle_key(KeyEvent::plain(Key::Char('X')));
        assert_eq!(editor.text(), "hello Xworld");
    }

    #[test]
    fn history_stores_expanded_paste_body() {
        let mut editor = editor();
        let big = "secret\n".repeat(12);
        editor.insert_paste(&big);
        let action = editor.handle_key(KeyEvent::plain(Key::Enter));
        assert!(matches!(action, EditorAction::Submit(_)));
        // The recalled entry must carry the paste body, not the marker.
        let entries = editor.history_entries();
        assert_eq!(entries.first().map(String::as_str), Some(big.as_str()));
        assert!(!entries[0].contains("[paste #"));
    }

    #[test]
    fn stale_trigger_does_not_panic_on_accept() {
        let mut editor = editor();
        editor.set_provider(Box::new(crate::autocomplete::FuzzyProvider::new(vec![
            crate::autocomplete::Completion {
                label: "src/main.rs".into(),
                description: None,
                insert: "src/main.rs ".into(),
            },
        ])));
        editor.insert_text("x @src");
        assert!(editor.popup_open());
        // Move the cursor left of the trigger without typing: the popup
        // goes stale, and accepting must close it, never invert a range.
        editor.handle_key(KeyEvent::plain(Key::Home));
        let action = editor.handle_key(KeyEvent::plain(Key::Tab));
        assert_eq!(action, EditorAction::Handled);
        assert!(!editor.popup_open(), "stale popup dropped");
        assert_eq!(editor.text(), "x @src", "buffer untouched");
    }

    #[test]
    fn history_recall_with_draft() {
        let mut editor = editor();
        editor.remember_history("first");
        editor.remember_history("second");
        type_string(&mut editor, "draft");
        assert!(editor.history_previous());
        assert_eq!(editor.text(), "second");
        assert!(editor.history_previous());
        assert_eq!(editor.text(), "first");
        assert!(editor.history_next());
        assert_eq!(editor.text(), "second");
        assert!(editor.history_next());
        assert_eq!(editor.text(), "draft", "draft restored at end");
        assert!(!editor.history_next());
    }

    #[test]
    fn large_paste_collapses_and_expands() {
        let mut editor = editor();
        let big = "x".repeat(1500);
        editor.insert_paste(&big);
        assert!(
            editor.text().contains("[paste #1 1500 chars]"),
            "marker in buffer: {:?}",
            editor.text()
        );
        assert_eq!(editor.expand_markers(editor.text().as_str()), big);
    }

    #[test]
    fn multiline_paste_collapses_by_lines() {
        let mut editor = editor();
        let big = "line\n".repeat(15);
        editor.insert_paste(&big);
        assert!(editor.text().contains("[paste #1 +15 lines]"));
    }

    #[test]
    fn render_draws_frame_and_prompt() {
        let mut editor = editor();
        type_string(&mut editor, "hi");
        let rows = editor.render_box(20, 24);
        assert_eq!(strip_ansi(&rows[0]), "╭──────────────────╮");
        assert_eq!(
            strip_ansi(&rows[1]),
            "│ ❯ hi             │",
            "cursor at end: {:?}",
            rows[1]
        );
        assert_eq!(strip_ansi(&rows[2]), "╰──────────────────╯");
    }

    #[test]
    fn render_embeds_cursor_marker() {
        let mut editor = editor();
        type_string(&mut editor, "abc");
        editor.handle_key(KeyEvent::plain(Key::Left));
        let rows = editor.render_box(20, 24);
        assert!(
            rows[1].contains(CURSOR_MARKER),
            "marker present in: {:?}",
            rows[1]
        );
        assert_eq!(width::width(&strip_ansi(&rows[1])), 20);
    }

    #[test]
    fn long_input_wraps_into_visual_rows() {
        let mut editor = editor();
        type_string(&mut editor, "aaaaaaaaaaaaaaaaaaaaaa"); // 22 chars
        let rows = editor.render_box(20, 24);
        // Body budget: 20 - 2 - 2 - 2 = 14 → two visual rows + borders.
        assert_eq!(rows.len(), 4, "wrapped into 2 body rows: {rows:?}");
        assert!(strip_ansi(&rows[1]).starts_with("│ ❯ aaaaaaaaa"));
    }

    #[test]
    fn vertical_movement_walks_visual_rows() {
        let mut editor = editor();
        // 20 a's wrap into two visual rows at body budget 14 (width 20).
        editor.insert_text("aaaaaaaaaaaaaaaaaaaa\nb");
        let _ = editor.render_box(20, 24);
        // insert_text leaves the cursor at the end of line 1.
        assert!(editor.move_up(), "moves to wrapped second row");
        assert!(editor.move_up(), "moves to the first visual row");
        assert!(!editor.move_up(), "first visual row falls through");
        assert!(editor.move_down(), "moves into wrapped second row");
        assert!(editor.move_down(), "moves to logical line 1");
        assert!(!editor.move_down(), "stays on last row");
    }

    #[test]
    fn popup_opens_on_slash_and_accepts() {
        let mut editor = editor();
        editor.set_provider(Box::new(crate::autocomplete::FuzzyProvider::new(vec![
            crate::autocomplete::Completion {
                label: "help".into(),
                description: None,
                insert: "/help ".into(),
            },
        ])));
        type_string(&mut editor, "/h");
        assert!(editor.popup_open(), "popup after slash token");
        let action = editor.handle_key(KeyEvent::plain(Key::Enter));
        assert_eq!(action, EditorAction::Submit("/help ".to_string()));
    }

    #[test]
    fn popup_navigation_and_escape() {
        let mut editor = editor();
        editor.set_provider(Box::new(crate::autocomplete::FuzzyProvider::new(vec![
            crate::autocomplete::Completion {
                label: "help".into(),
                description: None,
                insert: "/help ".into(),
            },
        ])));
        type_string(&mut editor, "/h");
        assert_eq!(
            editor.handle_key(KeyEvent::plain(Key::Esc)),
            EditorAction::Handled
        );
        assert!(!editor.popup_open());
        assert_eq!(editor.text(), "/h", "escape keeps the token");
    }
}
