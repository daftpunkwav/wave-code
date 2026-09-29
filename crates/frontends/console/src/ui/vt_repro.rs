//! Reproduction rig for the duplicated-input-box artifact: the real
//! [`ConsoleUi`] is driven through key sequences while a miniature VT
//! interpreter materializes every rendered frame onto a simulated
//! screen (viewport + scrollback). The tests then count editor boxes
//! on the materialized screen — the artifact is two copies of the
//! input region where exactly one may exist.

use super::*;
use crate::theme;
use std::path::PathBuf;
use test_support::{NullStatus, TestLink};
use tui_engine::keys::{Key, KeyEvent};

// ---------------------------------------------------------------------
// Miniature VT interpreter
// ---------------------------------------------------------------------

/// One materialized screen: a character grid plus native scrollback.
/// Handles exactly the escape vocabulary the screen renderer emits
/// (CUP/CUU/CUD, CR, LF, EL/ED, 2J/3J) with xterm semantics for
/// wrap-pending and bottom-line scrolling.
struct Vt {
    columns: usize,
    height: usize,
    cells: Vec<Vec<char>>,
    scrollback: Vec<String>,
    row: usize,
    col: usize,
    wrap_pending: bool,
}

impl Vt {
    fn new(columns: usize, height: usize) -> Self {
        Self {
            columns,
            height,
            cells: vec![vec![' '; columns]; height],
            scrollback: Vec::new(),
            row: 0,
            col: 0,
            wrap_pending: false,
        }
    }

    fn feed(&mut self, input: &[u8]) {
        let text = String::from_utf8_lossy(input);
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                self.put_char(c);
                continue;
            }
            match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut intermediates = String::new();
                    let final_byte = loop {
                        match chars.next() {
                            Some(ch) if ('\x30'..='\x3f').contains(&ch) => params.push(ch),
                            Some(ch) if ('\x20'..='\x2f').contains(&ch) => intermediates.push(ch),
                            Some(ch) => break ch,
                            None => return,
                        }
                    };
                    self.csi(&params, &intermediates, final_byte);
                }
                Some(']') => {
                    // OSC: swallow through BEL or ST (ESC \).
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some('(') | Some(')') => {
                    chars.next(); // charset designation: one byte
                }
                _ => {}
            }
        }
    }

    fn csi(&mut self, params: &str, intermediates: &str, final_byte: char) {
        if !intermediates.is_empty() {
            return;
        }
        let numbers: Vec<usize> = params
            .split(';')
            .map(|part| part.parse::<usize>().unwrap_or(0))
            .collect();
        let n = |index: usize, default: usize| {
            numbers
                .get(index)
                .copied()
                .filter(|v| *v != 0)
                .unwrap_or(default)
        };
        match final_byte {
            'H' | 'f' => {
                let r = n(0, 1).saturating_sub(1).min(self.height - 1);
                let c = n(1, 1).saturating_sub(1).min(self.columns - 1);
                self.row = r;
                self.col = c;
                self.wrap_pending = false;
            }
            'A' => {
                self.row -= n(0, 1).min(self.row);
                self.wrap_pending = false;
            }
            'B' => {
                self.row += n(0, 1).min(self.height - 1 - self.row);
                self.wrap_pending = false;
            }
            'K' => {
                // EL 0: erase from the cursor to end of line.
                for cell in &mut self.cells[self.row][self.col..] {
                    *cell = ' ';
                }
                self.wrap_pending = false;
            }
            'J' => match numbers.first().copied().unwrap_or(0) {
                0 => {
                    for cell in &mut self.cells[self.row][self.col..] {
                        *cell = ' ';
                    }
                    for cells in &mut self.cells[self.row + 1..] {
                        cells.fill(' ');
                    }
                }
                2 => {
                    for cells in &mut self.cells {
                        cells.fill(' ');
                    }
                }
                3 => self.scrollback.clear(),
                _ => {}
            },
            _ => {}
        }
    }

    fn put_char(&mut self, c: char) {
        if c == '\r' {
            self.col = 0;
            self.wrap_pending = false;
            return;
        }
        if c == '\n' {
            self.wrap_pending = false;
            if self.row + 1 >= self.height {
                self.scroll_up();
            } else {
                self.row += 1;
            }
            return;
        }
        if c.is_control() {
            return;
        }
        if self.wrap_pending {
            self.wrap_pending = false;
            if self.row + 1 >= self.height {
                self.scroll_up();
            } else {
                self.row += 1;
            }
            self.col = 0;
        }
        let w = char_width(c);
        if self.col >= self.columns {
            // Character cannot fit: the renderer truncates first, so
            // this only happens on a bug.
            self.wrap_pending = true;
            return;
        }
        if self.col + w > self.columns {
            self.wrap_pending = true;
            return;
        }
        for (offset, cell) in self.cells[self.row][self.col..].iter_mut().enumerate() {
            if offset < w {
                *cell = c;
            }
        }
        self.col += w;
        if self.col >= self.columns {
            self.wrap_pending = true;
        }
    }

    fn scroll_up(&mut self) {
        let top: String = self.cells[0].iter().collect();
        self.scrollback.push(top);
        self.cells.remove(0);
        self.cells.push(vec![' '; self.columns]);
    }

    /// The whole materialized surface: scrollback, then viewport.
    /// Wide characters are stored as two identical cells; the visual
    /// views fold them back so text assertions see one glyph.
    fn all_text(&self) -> String {
        let viewport: Vec<String> = self
            .cells
            .iter()
            .map(|cells| cells.iter().collect())
            .collect();
        self.scrollback
            .iter()
            .chain(viewport.iter())
            .map(|line| fold_wide_cells(line))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Collapse the doubled cells the grid uses to model wide glyphs back
/// into single characters.
fn fold_wide_cells(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if char_width(c) == 2 && chars.peek() == Some(&c) {
            chars.next();
        }
    }
    out
}

/// Display width good enough for the rig: ASCII is one cell, the CJK
/// range the draft text uses is two.
fn char_width(c: char) -> usize {
    let code = c as u32;
    if (0x1100..=0x115F).contains(&code)
        || (0x2E80..=0xA4CF).contains(&code)
        || (0xAC00..=0xD7A3).contains(&code)
        || (0xF900..=0xFAFF).contains(&code)
        || (0xFE30..=0xFE4F).contains(&code)
        || (0xFF00..=0xFF60).contains(&code)
        || (0xFFE0..=0xFFE6).contains(&code)
    {
        2
    } else {
        1
    }
}

// ---------------------------------------------------------------------
// UI rig: the run loop, condensed
// ---------------------------------------------------------------------

struct Rig {
    ui: ConsoleUi,
    vt: Vt,
    columns: usize,
    rows: usize,
}

impl Rig {
    fn new(columns: usize, rows: usize) -> Self {
        theme::set(theme::Theme::dark());
        let mut ui = ConsoleUi::new(
            Box::new(TestLink::new()),
            &UiContext {
                model_name: "test-model".to_string(),
                provider_id: String::new(),
                thinking_effort: None,
                thinking_levels: Vec::new(),
                cwd: PathBuf::from("/home/user/work/proj"),
                permission_mode: "auto".to_string(),
                skill_names: Vec::new(),
                mcp_servers: vec!["fs".to_string()],
                memory_files: Vec::new(),
                status: Arc::new(NullStatus),
                session_id: "session-0001".to_string(),
                session_title: None,
                model_entries: Vec::new(),
                home: None,
                update_notice: None,
                redactor: None,
            },
            "0.1.0",
        );
        ui.settings = crate::settings::SharedSettings::without_persistence(Default::default());
        let mut rig = Self {
            ui,
            vt: Vt::new(columns + 2, rows), // gutter margin on both sides
            columns,
            rows,
        };
        rig.paint();
        rig
    }

    /// One rendered frame, materialized on the simulated screen.
    fn paint(&mut self) {
        let mut out = Vec::new();
        self.ui.render(&mut out, self.columns, self.rows).unwrap();
        self.vt.feed(&out);
    }

    /// One key through the real handler chain, then the repaint the
    /// run loop performs after every key.
    fn key(&mut self, key: Key) {
        let flow = self.ui.handle_key(KeyEvent::plain(key));
        assert_eq!(flow, Flow::Continue);
        self.paint();
    }

    fn text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(Key::Char(c));
        }
    }

    /// A wire event plus the repaint the run loop performs for it.
    fn wire(&mut self, msg: EventMsg) {
        if self.ui.handle_wire_event(&msg) && self.ui.render_due() {
            self.ui.note_rendered();
            self.paint();
        }
    }

    /// The 100 ms tick branch of the run loop.
    fn tick(&mut self) {
        let _ = self.ui.poll_shell();
        let _ = self.ui.poll_btw();
        let _ = self.ui.poll_update_notice();
        let _ = self.ui.poll_statusline();
        if self.ui.needs_tick_render() {
            self.paint();
        }
    }

    /// The top-of-loop drain the run loop performs each iteration
    /// (session launches repaint through a full redraw).
    fn drain_launch(&mut self) {
        if self.ui.take_pending_launch() {
            self.paint();
        }
    }

    fn editor_box_count(&self) -> usize {
        count_occurrences(&self.vt.all_text(), '╭')
    }

    fn draft_copies(&self, draft: &str) -> usize {
        count_str_occurrences(&self.vt.all_text(), draft)
    }
}

fn count_occurrences(haystack: &str, needle: char) -> usize {
    haystack.chars().filter(|c| *c == needle).count()
}

fn count_str_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

fn grow_transcript(ui: &mut ConsoleUi, lines: usize) {
    for i in 0..lines {
        ui.push_status(
            &format!("history line {i:03} of the earlier session"),
            false,
        );
    }
}

// ---------------------------------------------------------------------
// Reproduction scenarios
// ---------------------------------------------------------------------

/// Baseline: with a tall transcript, opening the slash popup and
/// leaving it open keeps exactly one editor box.
#[test]
fn slash_popup_open_paints_one_editor_box() {
    let mut rig = Rig::new(80, 24);
    grow_transcript(&mut rig.ui, 30);
    rig.paint();
    rig.text("/he");
    assert!(rig.ui.editor.popup_open(), "popup should be open");
    assert_eq!(rig.editor_box_count(), 1, "{}", rig.vt.all_text());
}

/// The reported artifact: a slash popup that is dismissed (Esc) with a
/// tall transcript behind it must not leave a second editor box.
#[test]
fn slash_popup_escape_keeps_one_editor_box() {
    let mut rig = Rig::new(80, 24);
    grow_transcript(&mut rig.ui, 30);
    rig.paint();
    rig.text("/he");
    rig.key(Key::Esc);
    rig.text("you all what tools");
    assert_eq!(rig.editor_box_count(), 1, "{}", rig.vt.all_text());
    assert_eq!(
        rig.draft_copies("you all what tools"),
        1,
        "{}",
        rig.vt.all_text()
    );
}

/// Popup dismissal while streaming keeps appending transcript rows:
/// the frame grows as the popup closes (the diff path, not the shrink
/// path).
#[test]
fn popup_escape_during_streaming_keeps_one_editor_box() {
    let mut rig = Rig::new(80, 24);
    grow_transcript(&mut rig.ui, 20);
    rig.paint();
    rig.text("/he");
    for i in 0..10 {
        rig.wire(EventMsg::Warning {
            message: format!("streamed status row {i} while the popup is open"),
        });
        rig.tick();
    }
    rig.key(Key::Esc);
    for i in 0..10 {
        rig.wire(EventMsg::Warning {
            message: format!("post-escape streamed status row {i}"),
        });
        rig.tick();
    }
    assert_eq!(rig.editor_box_count(), 1, "{}", rig.vt.all_text());
}

/// A modal dialog (theme picker style) opened from a slash command and
/// dismissed again must not duplicate the input region.
#[test]
fn slash_dialog_open_close_keeps_one_editor_box() {
    let mut rig = Rig::new(80, 24);
    grow_transcript(&mut rig.ui, 30);
    rig.paint();
    rig.text("/theme");
    rig.key(Key::Enter);
    rig.paint();
    assert!(rig.ui.dialog.is_some(), "theme dialog open");
    rig.key(Key::Esc);
    assert!(rig.ui.dialog.is_none(), "dialog dismissed");
    assert_eq!(rig.editor_box_count(), 1, "{}", rig.vt.all_text());
}

/// The /sessions resume path: dialog select swaps the session (full
/// redraw through invalidate) and must not strand a second editor.
#[test]
fn session_resume_keeps_one_editor_box() {
    let mut rig = Rig::new(80, 24);
    grow_transcript(&mut rig.ui, 30);
    rig.paint();
    rig.ui.set_factory(Arc::new(|spec: &crate::ui::LaunchSpec| {
        let ctx = crate::ui::UiContext {
            model_name: "restored-model".to_string(),
            provider_id: String::new(),
            thinking_effort: None,
            thinking_levels: Vec::new(),
            cwd: PathBuf::from("/home/user/work/proj"),
            permission_mode: "auto".to_string(),
            skill_names: Vec::new(),
            mcp_servers: vec!["fs".to_string()],
            memory_files: Vec::new(),
            status: Arc::new(NullStatus),
            session_id: "session-0002".to_string(),
            session_title: Some("restored".to_string()),
            model_entries: Vec::new(),
            home: None,
            update_notice: None,
            redactor: None,
        };
        let _ = spec;
        Ok(crate::ui::SessionLaunch {
            link: Box::new(TestLink::new()),
            ctx,
            history: (0..12)
                .map(|i| (i % 2 == 1, format!("restored line {i}")))
                .collect(),
        })
    }));
    rig.text("/sessions");
    rig.key(Key::Enter);
    rig.paint();
    rig.drain_launch();
    rig.text("you all what tools");
    assert_eq!(rig.editor_box_count(), 1, "{}", rig.vt.all_text());
    assert_eq!(
        rig.draft_copies("you all what tools"),
        1,
        "{}",
        rig.vt.all_text()
    );
}

/// Typing CJK text (IME produces the final characters one per key)
/// while the popup cycles open and closed.
#[test]
fn cjk_typing_with_popup_cycles_keeps_one_editor_box() {
    let mut rig = Rig::new(80, 24);
    grow_transcript(&mut rig.ui, 30);
    rig.paint();
    rig.text("/");
    rig.key(Key::Esc);
    rig.text("你都什么工具");
    assert_eq!(rig.editor_box_count(), 1, "{}", rig.vt.all_text());
    assert_eq!(rig.draft_copies("你都什么工具"), 1, "{}", rig.vt.all_text());
}
