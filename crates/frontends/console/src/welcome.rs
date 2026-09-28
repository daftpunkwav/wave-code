/*!
 * @file Welcome
 * @description The branded welcome card shown at session start.
 *
 * Responsibilities:
 * - Render the figlet wordmark over the braille oscilloscope trace.
 * - Display session facts as a left-aligned info grid.
 * - Stir the trace on resizes (ripple animation).
 *
 * This module must not depend on: runtime or capability crates.
 */

//! The welcome card: the `slant` figlet wordmark fading primary→accent,
//! sitting directly on the braille oscilloscope trace (the wave flows
//! out of the letters), then the left-aligned info grid. Resizing stirs
//! the trace with an easing tail before it settles.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::theme::{self, Token};
use tui_engine::component::{Component, Segment};
use tui_engine::width;

/// How long the resize ripple flows before the wave settles.
const RIPPLE_DURATION: Duration = Duration::from_millis(1400);

/// The `slant` figlet wordmark (generated once with pyfiglet; keep in
/// sync with `pyfiglet.figlet_format("WAVECODE", font="slant")`).
pub const LOGO: [&str; 5] = [
    r#" _       _____ _    __________________  ____  ______"#,
    r#"| |     / /   | |  / / ____/ ____/ __ \/ __ \/ ____/"#,
    r#"| | /| / / /| | | / / __/ / /   / / / / / / / __/   "#,
    r#"| |/ |/ / ___ | |/ / /___/ /___/ /_/ / /_/ / /___   "#,
    r#"|__/|__/_/  |_|___/_____/\____/\____/_____/_____/   "#,
];
/// Minimum inner width that fits the figlet wordmark.
const LOGO_MIN_WIDTH: usize = 56;

/// Session facts the welcome card displays.
#[derive(Debug, Clone)]
pub struct WelcomeInfo {
    /// Product version string.
    pub version: String,
    /// Model display name.
    pub model: String,
    /// Permission mode display label.
    pub mode: String,
    /// Working directory.
    pub cwd: String,
    /// Connected MCP server names (empty when none).
    pub mcp_servers: Vec<String>,
    /// Git branch label (None outside a repository).
    pub branch: Option<String>,
}

/// Index of the first oscilloscope row within the card at `inner`.
fn wave_offset(inner: usize) -> usize {
    // leading blank + logo rows (figlet or the one-line wordmark).
    1 + if inner >= LOGO_MIN_WIDTH {
        LOGO.len()
    } else {
        1
    }
}

/// Build the welcome card lines at `width`.
pub fn render(info: &WelcomeInfo, width: usize) -> Vec<String> {
    let theme = theme::current();
    let palette = theme.palette();
    // Honor the real budget: a clamped-up `inner` (the old `.max(24)`)
    // emitted 24-column card lines into 1-2 column viewports.
    let inner = width.saturating_sub(2);

    let mut content: Vec<String> = Vec::new();
    content.push(String::new());
    if inner >= LOGO_MIN_WIDTH {
        // The wordmark fades from primary into accent, top to bottom.
        for (row, line) in LOGO.iter().enumerate() {
            let k = row as f32 / (LOGO.len() - 1) as f32;
            let c = lerp(palette.primary, palette.accent, k);
            content.push(center_line(
                &format!("\x1b[{}m{line}\x1b[0m", c.fg_params()),
                inner,
            ));
        }
    } else {
        content.push(center_line(
            &theme.bold(Token::Primary, "W A V E C O D E"),
            inner,
        ));
    }
    let wave = wave_banner(inner, 0.0, palette.primary, palette.accent);
    content.push(wave.0);
    content.push(wave.1);

    // One blank spacer keeps the info grid clear of the logo's trace.
    content.push(String::new());

    // Info grid, left-aligned: dim labels in a fixed column, values to
    // the right.
    let facts = [
        ("model", info.model.clone(), String::new()),
        (
            "dir",
            shorten_home(&info.cwd),
            info.branch.clone().unwrap_or_default(),
        ),
        ("mode", mode_row(info), String::new()),
    ];
    for (label, value, suffix) in facts {
        let mut line = theme.paint(Token::TextMuted, &format!("{label:<6}"));
        line.push_str(&theme.paint(Token::Text, &value));
        if !suffix.is_empty() {
            line.push_str(&theme.paint(
                Token::Accent,
                &format!("  {} {suffix}", crate::chrome::symbols::BRANCH),
            ));
        }
        content.push(line);
    }

    // One blank spacer keeps the transcript below from hugging the card.
    content.push(String::new());
    // Degenerate budgets (inner smaller than one fact row or the
    // wordmark) truncate per line instead of overflowing the frame.
    content
        .into_iter()
        .map(|line| width::truncate_to_width(&line, inner))
        .collect()
}

/// The interference-wave banner, drawn as a braille oscilloscope trace:
/// a 1-pixel sine line flowing through two braille rows (8 dot rows
/// tall), amplitude-modulated by a slow envelope so the wave breathes
/// in groups. Painted with a primary→accent→primary gradient per cell.
/// `phase` slides the trace to the right; the resize ripple animates it.
fn wave_banner(
    columns: usize,
    phase: f32,
    from: tui_engine::color::Color,
    to: tui_engine::color::Color,
) -> (String, String) {
    // Braille cell bit layout: rows 0-3 top to bottom, left/right.
    const DOT: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
    let pixel_count = columns * 2;
    // Per-pixel trace height: modulated sine through the vertical mid.
    let height = |x: usize| {
        let t = x as f32 + phase * 12.0;
        let envelope = 0.35 + 0.65 * (0.5 + 0.5 * (x as f32 * 0.02).sin());
        (3.5 - 3.2 * envelope * (t * 0.22).sin()).clamp(0.0, 7.0)
    };
    let mut top_bits = vec![0u32; columns];
    let mut bottom_bits = vec![0u32; columns];
    for x in 0..pixel_count {
        let cell = x / 2;
        let half = x % 2;
        // Trace point plus interpolated dots so steep segments stay
        // connected.
        let y0 = height(x);
        let y1 = height(x + 1);
        let (from_y, to_y) = if y1 >= y0 { (y0, y1) } else { (y1, y0) };
        let mut ys: Vec<i32> = ((from_y.round() as i32)..=(to_y.round() as i32)).collect();
        ys.push(y0.round() as i32);
        for y in ys {
            let y = y.clamp(0, 7) as usize;
            let bit = DOT[y % 4][half];
            if y < 4 {
                top_bits[cell] |= bit;
            } else {
                bottom_bits[cell] |= bit;
            }
        }
    }
    let paint = |bits: &[u32]| {
        bits.iter()
            .enumerate()
            .map(|(cell, cell_bits)| {
                let t = cell as f32 / (columns.saturating_sub(1)) as f32;
                // There-and-back easing: primary at the ends, accent
                // mid-span.
                let k = 0.5 * (1.0 - (t * std::f32::consts::TAU).cos());
                let c = lerp(from, to, k);
                format!(
                    "\x1b[{}m{}\x1b[0m",
                    c.fg_params(),
                    char::from_u32(0x2800 + cell_bits).unwrap()
                )
            })
            .collect::<String>()
    };
    (paint(&top_bits), paint(&bottom_bits))
}

/// Linear interpolation between two colors.
fn lerp(
    from: tui_engine::color::Color,
    to: tui_engine::color::Color,
    k: f32,
) -> tui_engine::color::Color {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * k).round() as u8;
    tui_engine::color::Color::rgb(mix(from.r, to.r), mix(from.g, to.g), mix(from.b, to.b))
}

/// Center a line by its visible width.
fn center_line(line: &str, width: usize) -> String {
    center_line_fixed(line.to_string(), width::width(line), width)
}

/// Center a pre-painted line whose visible width is known.
fn center_line_fixed(line: String, visible: usize, width: usize) -> String {
    let pad = width.saturating_sub(visible) / 2;
    format!("{}{}", " ".repeat(pad), line)
}

/// The mode row: permission mode plus MCP server count and version.
fn mode_row(info: &WelcomeInfo) -> String {
    let mut row = info.mode.to_lowercase();
    if !info.mcp_servers.is_empty() {
        row.push_str(&format!(" · mcp {}", info.mcp_servers.len()));
    }
    row.push_str(&format!(" · v{}", info.version));
    row
}

/// Replace the home directory prefix with `~` when present.
fn shorten_home(path: &str) -> String {
    if let Some(home) = wavecode_config::home_dir()
        && !home.as_os_str().is_empty()
        && let Some(rest) = path.strip_prefix(&home.to_string_lossy().to_string())
    {
        let rest = rest.trim_start_matches(['/', '\\']);
        return format!("~/{rest}");
    }
    path.to_string()
}

/// A component wrapper for the welcome card (renders once, cached).
/// A width change stirs the wave: it slides right with an easing tail
/// before settling again.
pub struct Welcome {
    info: WelcomeInfo,
    width: usize,
    lines: Option<Segment>,
    ripple: Option<Instant>,
}

impl Welcome {
    /// A card for `info`.
    pub fn new(info: WelcomeInfo) -> Self {
        Self {
            info,
            width: 0,
            lines: None,
            ripple: None,
        }
    }

    /// True while the resize ripple is still flowing (the host must
    /// keep ticking so the animation can play out).
    pub fn is_rippling(&self) -> bool {
        self.ripple.is_some()
    }
}

impl Component for Welcome {
    fn render(&mut self, width: usize) -> Segment {
        if self.width != width {
            self.width = width;
            self.lines = None;
            self.ripple = Some(Instant::now());
        }
        let Some(started) = self.ripple else {
            let lines = self
                .lines
                .get_or_insert_with(|| Arc::new(render(&self.info, width)));
            return Arc::clone(lines);
        };
        let t = started.elapsed().as_secs_f32() / RIPPLE_DURATION.as_secs_f32();
        if t >= 1.0 {
            self.ripple = None;
            let lines = Arc::new(render(&self.info, width));
            self.lines = Some(Arc::clone(&lines));
            return lines;
        }
        // Ease-out cubic: the trace surges, then glides to a stop.
        let ease = 1.0 - (1.0 - t).powi(3);
        let mut frame = render(&self.info, width);
        let palette = theme::current().palette();
        let inner = width.saturating_sub(2);
        let (top, bottom) = wave_banner(
            inner,
            ease * std::f32::consts::TAU * 1.5,
            palette.primary,
            palette.accent,
        );
        let offset = wave_offset(inner);
        if frame.len() >= offset + 2 {
            frame[offset] = top;
            frame[offset + 1] = bottom;
        }
        Arc::new(frame)
    }

    fn invalidate(&mut self) {
        self.lines = None;
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use tui_engine::width::strip_ansi;

    fn info() -> WelcomeInfo {
        WelcomeInfo {
            version: "0.1.0".to_string(),
            model: "test-model".to_string(),
            mode: "Auto Mode".to_string(),
            cwd: "/tmp/project".to_string(),
            mcp_servers: vec!["a".to_string()],
            branch: Some("main".to_string()),
        }
    }

    #[test]
    fn card_shows_wordmark_and_info_grid() {
        theme::set(theme::Theme::dark());
        let mut card = Welcome::new(info());
        let lines = card.render(60);
        let text: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        let joined = text.join("\n");
        assert!(joined.contains(LOGO[0]), "{joined}");
        assert!(joined.contains("model"), "{joined}");
        assert!(joined.contains("test-model"), "{joined}");
        assert!(joined.contains("/tmp/project"), "{joined}");
        assert!(joined.contains("mcp 1"), "{joined}");
        assert!(joined.contains("v0.1.0"), "{joined}");
        assert!(joined.contains("auto mode"), "{joined}");
    }

    #[test]
    fn banner_is_braille_oscilloscope_trace() {
        theme::set(theme::Theme::dark());
        let mut card = Welcome::new(info());
        let lines = card.render(60);
        for row in [6usize, 7] {
            let banner = &lines[row];
            assert_eq!(width::width(banner), 58, "banner width: {banner:?}");
            // The gradient paints more than one distinct color.
            let starts: Vec<&str> = banner
                .match_indices("\x1b[")
                .map(|(i, _)| {
                    let rest = &banner[i..];
                    &rest[..rest.find('m').map(|m| m + 1).unwrap_or(0)]
                })
                .collect();
            let distinct: std::collections::HashSet<&str> = starts.into_iter().collect();
            assert!(distinct.len() > 2, "gradient colors: {distinct:?}");
            // Braille cells or silence only.
            assert!(
                strip_ansi(banner)
                    .chars()
                    .all(|c| ('\u{2800}'..='\u{28FF}').contains(&c) || c == ' '),
                "row {row}: {:?}",
                strip_ansi(banner)
            );
        }
        // The trace crosses the midline: both rows carry dots.
        assert!(
            strip_ansi(&lines[6])
                .chars()
                .any(|c| c != '\u{2800}' && c != ' ')
        );
        assert!(
            strip_ansi(&lines[7])
                .chars()
                .any(|c| c != '\u{2800}' && c != ' ')
        );
    }

    #[test]
    fn wordmark_is_centered() {
        theme::set(theme::Theme::dark());
        let mut card = Welcome::new(info());
        let lines = card.render(60);
        let plain = strip_ansi(&lines[1]);
        let pad = plain.len() - plain.trim_start().len();
        assert!(
            pad > 1 && pad < 10,
            "figlet wordmark centered, pad={pad}: {plain:?}"
        );
    }

    /// The info grid sits left-aligned one blank line below the trace.
    #[test]
    fn info_grid_is_left_aligned_below_a_spacer() {
        theme::set(theme::Theme::dark());
        let mut card = Welcome::new(info());
        let lines = card.render(60);
        let text: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(text[8].is_empty(), "spacer after the trace: {text:?}");
        for (row, label) in text[9..=11].iter().zip(["model", "dir", "mode"]) {
            assert!(row.starts_with(label), "{label} left-aligned: {row:?}");
        }
    }

    #[test]
    fn narrow_width_falls_back_to_spaced_wordmark() {
        theme::set(theme::Theme::dark());
        let mut card = Welcome::new(info());
        let lines = card.render(40);
        let joined = lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("W A V E C O D E"), "{joined}");
        assert!(
            !joined.contains(LOGO[0]),
            "no figlet below {LOGO_MIN_WIDTH}: {joined}"
        );
    }

    /// Manual visual check: `cargo test -p console-ui welcome_snapshot
    /// -- --ignored --nocapture` prints the card for eyeballing.
    #[test]
    #[ignore]
    fn welcome_snapshot() {
        theme::set(theme::Theme::dark());
        let mut card = Welcome::new(info());
        for line in card.render(60).iter() {
            println!("{line}");
        }
    }
}
