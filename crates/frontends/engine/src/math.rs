//! Minimal TeX-to-Unicode conversion for math segments.
//!
//! Terminal math renders as text: common commands map onto Unicode
//! (`\int` → `∫`, `\alpha` → `α`, `\times` → `×`), simple `^{...}` /
//! `_{...}` groups map onto superscript/subscript code points where a
//! glyph exists (digits, parentheses, operators, most Latin letters);
//! a glyph without a script code point keeps its full-size character,
//! and the `^` / `_` markers themselves never survive into the output
//! (`x^{q2}` renders `xq²`). `\frac{a}{b}` renders `(a)/(b)`,
//! `\sqrt{x}` renders `√(x)`. Unknown commands degrade to their name.

use crate::width;

/// Map one character to its superscript code point, if one exists.
fn superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        'A' => 'ᴬ',
        'B' => 'ᴮ',
        'D' => 'ᴰ',
        'E' => 'ᴱ',
        'G' => 'ᴳ',
        'H' => 'ᴴ',
        'I' => 'ᴵ',
        'J' => 'ᴶ',
        'K' => 'ᴷ',
        'L' => 'ᴸ',
        'M' => 'ᴹ',
        'N' => 'ᴺ',
        'O' => 'ᴼ',
        'P' => 'ᴾ',
        'R' => 'ᴿ',
        'T' => 'ᵀ',
        'U' => 'ᵁ',
        'V' => 'ⱽ',
        'W' => 'ᵂ',
        _ => return None,
    })
}

/// Map one character to its subscript code point, if one exists.
fn subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

/// Map `text` onto superscript code points; `None` when any character
/// is unmappable (the caller keeps the original notation).
pub fn to_superscript(text: &str) -> Option<String> {
    script_all(text, false)
}

/// Map `text` onto subscript code points; `None` when any character is
/// unmappable (the caller keeps the original notation).
pub fn to_subscript(text: &str) -> Option<String> {
    script_all(text, true)
}

/// Map `text` onto superscript/subscript code points; `None` when any
/// character is unmappable (the caller keeps the original notation).
fn script_all(text: &str, sub: bool) -> Option<String> {
    text.chars()
        .map(|c| if sub { subscript(c) } else { superscript(c) })
        .collect()
}

/// Greek letters and named operators.
fn command_glyph(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "rho" => "ρ",
        "sigma" => "σ",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "φ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "hbar" => "ℏ",
        "ell" => "ℓ",
        "times" => "×",
        "cdot" => "·",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "leq" => "≤",
        "le" => "≤",
        "geq" => "≥",
        "ge" => "≥",
        "neq" => "≠",
        "ne" => "≠",
        "approx" => "≈",
        "sim" => "∼",
        "equiv" => "≡",
        "propto" => "∝",
        "rightarrow" => "→",
        "to" => "→",
        "leftarrow" => "←",
        "gets" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" => "⇔",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "emptyset" => "∅",
        "varnothing" => "∅",
        "forall" => "∀",
        "exists" => "∃",
        "neg" => "¬",
        "land" => "∧",
        "lor" => "∨",
        "oplus" => "⊕",
        "otimes" => "⊗",
        "perp" => "⊥",
        "parallel" => "∥",
        "angle" => "∠",
        "triangle" => "△",
        "square" => "□",
        "degree" => "°",
        "circ" => "∘",
        "star" => "⋆",
        "ast" => "∗",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "bigcup" => "⋃",
        "bigcap" => "⋂",
        "bigoplus" => "⨁",
        "bigotimes" => "⨂",
        "ldots" => "…",
        "cdots" => "⋯",
        "dots" => "…",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "prime" => "′",
        "quad" => "  ",
        "qquad" => "    ",
        " " => " ",
        "," => " ",
        ";" => " ",
        ":" => " ",
        "!" => "",
        "left" => "",
        "right" => "",
        "displaystyle" => "",
        "limits" => "",
        "nolimits" => "",
        _ => return None,
    })
}

/// Word-like operators that render as themselves (roman, not italic).
fn function_name(name: &str) -> bool {
    matches!(
        name,
        "sin"
            | "cos"
            | "tan"
            | "cot"
            | "sec"
            | "csc"
            | "arcsin"
            | "arccos"
            | "arctan"
            | "sinh"
            | "cosh"
            | "tanh"
            | "log"
            | "ln"
            | "lg"
            | "exp"
            | "lim"
            | "max"
            | "min"
            | "sup"
            | "inf"
            | "det"
            | "dim"
            | "ker"
            | "deg"
            | "gcd"
            | "arg"
            | "mod"
            | "text"
            | "mathrm"
            | "mathbf"
            | "operatorname"
    )
}

/// Convert a TeX fragment to display text. Brace groups nest; depth is
/// tracked so a stray brace can never desynchronize the scan.
pub fn render(tex: &str) -> String {
    render_depth(tex, 0)
}

/// Render a block-level math segment (`$$...$$`) into display lines:
/// matrix environments (`\begin{pmatrix}...\end{pmatrix}` and
/// siblings) lay out as aligned columns between brackets; anything
/// else renders as one line through [`render`].
pub fn render_block(tex: &str, columns: usize) -> Vec<String> {
    if let Some((env, body)) = find_environment(tex)
        && let Some(rows) = matrix_rows(body, columns)
    {
        let (open, close) = brackets_for(env);
        return rows
            .iter()
            .map(|cells| {
                let inner = cells.join("  ");
                format!("{}{inner}{}", render(open), render(close))
            })
            .collect();
    }
    vec![render(tex)]
}

/// Find the first `\begin{env}...\end{env}` region; returns the
/// environment name and its body.
fn find_environment(tex: &str) -> Option<(&str, &str)> {
    let begin = tex.find("\\begin{")?;
    let after = &tex[begin + "\\begin{".len()..];
    let close = after.find('}')?;
    let env = &after[..close];
    let body_start = begin + "\\begin{".len() + close + 1;
    let end_marker = format!("\\end{{{env}}}");
    let body_end = tex[body_start..].find(&end_marker)? + body_start;
    Some((env, tex[body_start..body_end].trim()))
}

/// Bracket pair for a matrix environment (pmatrix → parentheses,
/// bmatrix/Bmatrix → brackets, vmatrix/Vmatrix → bars, plain matrix →
/// none).
fn brackets_for(env: &str) -> (&'static str, &'static str) {
    match env {
        "pmatrix" => ("(", ")"),
        "bmatrix" | "Bmatrix" => ("[", "]"),
        "vmatrix" | "Vmatrix" => ("|", "|"),
        _ => ("", ""),
    }
}

/// Split a matrix body into rows of rendered, width-padded cells; the
/// column grid spans the widest cell of each column. `None` when the
/// body has too many rows or columns for a terminal layout.
fn matrix_rows(body: &str, columns: usize) -> Option<Vec<Vec<String>>> {
    let rows: Vec<Vec<String>> = body
        .split("\\\\")
        .map(|row| row.split('&').map(|cell| render(cell.trim())).collect())
        .collect();
    if rows.is_empty() || rows.len() > 24 || rows.iter().any(|r| r.len() > 12) {
        return None;
    }
    let col_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![1usize; col_count];
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(width::width(cell));
        }
    }
    let total: usize = widths.iter().sum::<usize>() + 2 * col_count.saturating_sub(1);
    if total + 2 > columns {
        return None;
    }
    Some(
        rows.iter()
            .map(|row| {
                (0..col_count)
                    .map(|index| {
                        let cell = row.get(index).cloned().unwrap_or_default();
                        let pad = widths[index].saturating_sub(width::width(&cell));
                        format!("{cell}{}", " ".repeat(pad))
                    })
                    .collect()
            })
            .collect(),
    )
}

/// Recursion budget for nested groups: real TeX never nests deeper
/// than a handful of levels, and the budget caps both the stack and
/// the re-tokenization cost of adversarial `{{{...` input.
const MAX_DEPTH: usize = 32;

fn render_depth(tex: &str, depth: usize) -> String {
    if depth > MAX_DEPTH {
        // Absurd nesting: emit the fragment verbatim instead of
        // recursing further (model-sourced input must never be able to
        // overflow the stack through the renderer).
        return tex.to_string();
    }
    let chars: Vec<char> = tex.chars().collect();
    let mut out = String::with_capacity(tex.len());
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            // Superscript / subscript: a brace group or a single token
            // maps onto script code points when every glyph maps;
            // otherwise the original notation is kept verbatim.
            '^' | '_' => {
                let sub = c == '_';
                if let Some((group, next)) = read_group(&chars, i + 1) {
                    // Best effort: map every glyph that has a script
                    // code point, keep the rest full-size. Raw `^{...}`
                    // notation never survives into the output.
                    let scripted: String = render_depth(&group, depth + 1)
                        .chars()
                        .map(|ch| {
                            if sub {
                                subscript(ch).unwrap_or(ch)
                            } else {
                                superscript(ch).unwrap_or(ch)
                            }
                        })
                        .collect();
                    out.push_str(&scripted);
                    i = next;
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            '\\' => {
                let start = i + 1;
                let mut end = start;
                while end < chars.len() && chars[end].is_ascii_alphabetic() {
                    end += 1;
                }
                if end == start {
                    // Escaped punctuation (\{, \}, \\, \, ...): emit the
                    // character itself. A `\\` row break renders as one
                    // blank — a literal newline would split the logical
                    // line the renderer assumed.
                    if start < chars.len() {
                        let esc = chars[start];
                        match esc {
                            '{' => out.push('{'),
                            '}' => out.push('}'),
                            '\\' => out.push(' '),
                            _ => out.push(esc),
                        }
                        i = start + 1;
                    } else {
                        i = start;
                    }
                    continue;
                }
                let name: String = chars[start..end].iter().collect();
                i = end;
                // Argument-taking forms first, then the glyph table.
                if name == "frac" {
                    match read_group(&chars, i) {
                        Some((numerator, next)) => {
                            let numerator = render_depth(&numerator, depth + 1);
                            i = next;
                            match read_group(&chars, i) {
                                Some((denominator, next)) => {
                                    out.push('(');
                                    out.push_str(&numerator);
                                    out.push_str(")/(");
                                    out.push_str(&render_depth(&denominator, depth + 1));
                                    out.push(')');
                                    i = next;
                                }
                                None => {
                                    // Streaming prefix: numerator only.
                                    out.push('(');
                                    out.push_str(&numerator);
                                    out.push_str(")/");
                                }
                            }
                        }
                        None => out.push_str("frac"),
                    }
                    continue;
                }
                if name == "sqrt" {
                    // Optional root degree: \sqrt[3]{x}.
                    let mut degree = String::new();
                    if i < chars.len()
                        && chars[i] == '['
                        && let Some(close) = chars[i..].iter().position(|c| *c == ']')
                    {
                        degree = chars[i + 1..i + close].iter().collect();
                        i += close + 1;
                    }
                    match read_group(&chars, i) {
                        Some((radicand, next)) => {
                            if !degree.is_empty() {
                                match script_all(&render_depth(&degree, depth + 1), true) {
                                    Some(scripted) => out.push_str(&scripted),
                                    None => {
                                        out.push('^');
                                        out.push('(');
                                        out.push_str(&render_depth(&degree, depth + 1));
                                        out.push(')');
                                    }
                                }
                            }
                            out.push_str("√(");
                            out.push_str(&render_depth(&radicand, depth + 1));
                            out.push(')');
                            i = next;
                        }
                        None => out.push_str("sqrt"),
                    }
                    continue;
                }
                if let Some(glyph) = command_glyph(&name) {
                    out.push_str(glyph);
                } else if matches!(name.as_str(), "text" | "mathrm" | "mathbf" | "operatorname") {
                    // Roman-text commands render ONLY their argument:
                    // the command name itself never shows.
                    if let Some((group, next)) = read_group(&chars, i) {
                        out.push_str(&render_depth(&group, depth + 1));
                        i = next;
                    }
                } else if function_name(&name) {
                    out.push_str(&name);
                    // A braced argument gets a thin space (`\sin{x}` reads
                    // "sin x", not "sinx").
                    if chars.get(i) == Some(&'{')
                        && let Some((group, next)) = read_group(&chars, i)
                    {
                        out.push(' ');
                        out.push_str(&render_depth(&group, depth + 1));
                        i = next;
                    }
                } else {
                    // Unknown command: keep its name as plain text.
                    out.push_str(&name);
                }
            }
            '{' | '}' => {
                // Bare group braces vanish (grouping only).
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Read a `{...}` group starting at `start`, or a single character.
/// Returns the group content and the index after it.
fn read_group(chars: &[char], start: usize) -> Option<(String, usize)> {
    if start >= chars.len() {
        return None;
    }
    if chars[start] != '{' {
        // A single token: a command or one character.
        if chars[start] == '\\' {
            let mut end = start + 1;
            while end < chars.len() && chars[end].is_ascii_alphabetic() {
                end += 1;
            }
            if end == start + 1 && end < chars.len() {
                end += 1; // escaped single char
            }
            return Some((chars[start..end].iter().collect(), end));
        }
        return Some((chars[start].to_string(), start + 1));
    }
    let mut depth = 0usize;
    for (offset, c) in chars[start..].iter().enumerate() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset;
                    return Some((chars[start + 1..end].iter().collect(), end + 1));
                }
            }
            _ => {}
        }
    }
    // Unclosed group (streaming prefix): take everything to the end.
    Some((chars[start + 1..].iter().collect(), chars.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greek_and_operators_map() {
        assert_eq!(render(r"\alpha + \beta"), "α + β");
        assert_eq!(render(r"E = mc^2"), "E = mc²");
        assert_eq!(render(r"\int_{-\infty}^{\infty}"), "∫₋∞∞");
    }

    #[test]
    fn scripts_map_or_fall_back() {
        assert_eq!(render("x^{22}"), "x²²");
        assert_eq!(render("H_{2}O"), "H₂O");
        // Every glyph inside the group carries a script code point, so
        // the whole `\frac` result scripts; the `^{}` markers vanish.
        assert_eq!(render("a^{\\frac{1}{2}}"), "a⁽¹⁾/⁽²⁾");
    }

    #[test]
    fn frac_and_sqrt_read_as_text() {
        assert_eq!(render(r"\frac{n(n+1)}{2}"), "(n(n+1))/(2)");
        assert_eq!(render(r"\sqrt{\pi}"), "√(π)");
    }

    #[test]
    fn unclosed_groups_degrade_gracefully() {
        // A streaming prefix must never panic and must stay readable.
        assert_eq!(render("x^{12"), "x¹²");
        assert_eq!(render(r"\alpha"), "α");
    }

    #[test]
    fn unknown_commands_keep_their_name() {
        assert_eq!(render(r"\foo x"), "foo x");
        assert_eq!(render(r"\sin \theta"), "sin θ");
    }

    /// Adversarial nesting must not overflow the stack: past the depth
    /// budget the fragment is emitted verbatim.
    #[test]
    fn deep_nesting_hits_the_depth_budget() {
        let mut tex = String::from("1");
        for _ in 0..4_000 {
            tex = format!(r"\frac{{{tex}}}{{2}}");
        }
        let rendered = render(&tex); // must not overflow the stack
        assert!(rendered.starts_with('('), "{rendered:?}");
    }

    /// A `\\` row break renders as one blank, never as a literal
    /// newline (the renderer contract is one string per row).
    #[test]
    fn row_break_renders_as_a_blank() {
        assert_eq!(render(r"a \\ b"), "a   b");
    }

    /// Roman-text commands show only their argument; named functions
    /// keep their name with a thin space before braced arguments.
    #[test]
    fn text_commands_hide_their_name_and_functions_keep_a_space() {
        assert_eq!(render(r"\text{acceleration}"), "acceleration");
        assert_eq!(render(r"\mathrm{d}x"), "dx");
        assert_eq!(render(r"\sin{x}"), "sin x");
        assert_eq!(render(r"\sin\theta"), "sinθ");
    }
}
