//! Tests for the markdown renderer ([`crate::markdown`]), grouped by
//! concern: caching, fenced blocks, tables, inline styling, CJK
//! handling, and vertical rhythm.

pub(crate) use super::*;
pub(crate) use crate::color::Color;
pub(crate) use crate::width::strip_ansi;

mod cache;
mod cjk;
mod code_blocks;
mod inline;
mod rhythm;
mod tables;

fn renderer() -> Markdown {
    Markdown::new(MarkdownStyle::default(), Box::new(PlainHighlighter))
}

/// One entry per display column: wide glyphs repeat into both of
/// their cells, so plain indexing matches what a terminal shows.
fn display_columns(line: &str) -> Vec<char> {
    let mut cols = Vec::new();
    for ch in line.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(1)
            .max(1);
        cols.push(ch);
        for _ in 1..w {
            cols.push(ch);
        }
    }
    cols
}
