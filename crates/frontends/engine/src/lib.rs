/*!
 * @file TuiEngine
 * @description Inline terminal rendering engine: components, editor,
 * markdown, and the differential screen renderer.
 *
 * Responsibilities:
 * - Own the render model: components produce ANSI line arrays; the
 *   screen diffs them into minimal terminal writes.
 * - Provide the built-in components the console UI composes: editor,
 *   select list, loader, text, borders, markdown.
 * - Normalize key input and visible-width math on grapheme boundaries.
 *
 * This crate is a pure library with zero internal workspace
 * dependencies. Semantic theming and session wiring live in the
 * application layer above it. It must not depend on: any workspace
 * crate, the agent core, or an async runtime.
 */

//! Inline terminal rendering engine.
//!
//! The render model mirrors the reference design: every component
//! renders to one ANSI string per terminal row for a given width, and
//! the screen layer diffs those arrays between frames, rewriting only
//! what changed while preserving native scrollback (no alternate
//! screen). Frames are wrapped in synchronized-output markers so
//! partial frames never flicker.

pub mod autocomplete;
pub mod border;
pub mod color;
pub mod component;
pub mod editor;
pub mod fuzzy;
pub mod keys;
pub mod loader;
pub mod markdown;
pub mod paste_burst;
pub mod sanitize;
pub mod screen;
pub mod select_list;
pub mod terminal;
pub mod text;
pub mod width;

pub use autocomplete::{Completion, CompletionProvider};
pub use component::{Component, Container, Focusable};
pub use editor::{Editor, EditorAction, EditorStyle};
pub use keys::{Key, KeyEvent, Mods};
pub use screen::{Screen, ScreenOptions};

#[cfg(test)]
mod dependency_matrix_locked {
    use std::path::Path;

    /// The engine stays a pure library: every dependency table
    /// (`[dependencies]`, target sections, dev/build) must carry only
    /// known external crates. Matching every key in every table (not
    /// just `path =` in `[dependencies]`) closes both the
    /// `foo.workspace = true` bypass and the target-section bypass.
    /// Adding an internal dependency is a boundary change: update
    /// deliberately.
    #[test]
    fn engine_has_no_internal_dependencies() {
        const EXTERNAL: [&str; 5] = [
            "crossterm",
            "unicode-width",
            "unicode-segmentation",
            "pulldown-cmark",
            "libc",
        ];
        let manifest = include_str!("../Cargo.toml");
        let mut in_deps = false;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_deps = trimmed.contains("dependencies");
                continue;
            }
            if !in_deps || trimmed.starts_with('#') || trimmed.is_empty() {
                continue;
            }
            let raw_key = trimmed.split('=').next().map(str::trim).unwrap_or("");
            let key = raw_key.split('.').next().unwrap_or(raw_key);
            assert!(
                EXTERNAL.contains(&key),
                "unexpected dependency `{key}`: internal deps are forbidden"
            );
        }
        let _ = Path::new("Cargo.toml");
    }
}
