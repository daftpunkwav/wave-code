//! Minimal Mermaid flowchart rendering for fenced `mermaid` blocks.
//!
//! Supports `graph`/`flowchart` with a `TD`/`TB` direction: node shapes
//! (`[]`, `()`, `{}`, `(( ))`, `[[]]`) all collapse onto one
//! box-drawing box, edges ride `-->`, `---`, `-.->`, `==>` with
//! optional labels (`-->|text|` or `-- text -->`), and chained
//! statements (`a --> b --> c`) parse. Anything else — `LR`/`RL`/`BT`
//! directions, subgraphs, other diagram types — returns `None` so the
//! caller falls back to the source view. The render/source toggle is
//! process-global (Ctrl+M in the console), like the color depth.
//!
//! Layout is a layered flow: nodes stack in bands by longest-path
//! depth, edges route down the band gaps with a single elbow per edge
//! (pass-through edges run a straight vertical through the gaps they
//! cross). Same-layer edges are skipped. The diagram must fit `columns`
//! or nothing renders.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::width;

static RENDER: AtomicBool = AtomicBool::new(false);

/// True when mermaid fences render as diagrams.
pub fn render_enabled() -> bool {
    RENDER.load(Ordering::Relaxed)
}

/// Toggle diagram rendering for mermaid fences. Callers own cache
/// invalidation: every rendered Markdown instance holds the previous
/// mode in its cache, so a toggle must be followed by an invalidate
/// pass over live components (and the screen).
pub fn set_render_enabled(on: bool) {
    RENDER.store(on, Ordering::Relaxed);
}

/// Render `source` into diagram lines at most `columns` wide; `None`
/// when the diagram kind is unsupported or does not fit.
pub fn render_diagram(source: &str, columns: usize) -> Option<Vec<String>> {
    let keyword = source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("%%"))
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or("")
        .to_ascii_lowercase();
    match keyword.as_str() {
        "graph" | "flowchart" => layout(&parse(source)?, columns),
        // A state diagram is a flowchart with colon labels and `[*]`
        // terminators; the adapter rewrites it into flowchart syntax.
        "statediagram" | "statediagram-v2" => {
            let adapted = adapt_state_diagram(source)?;
            layout(&parse(&adapted)?, columns)
        }
        "sequencediagram" => {
            let sequence = parse_sequence(source)?;
            layout_sequence(&sequence, columns)
        }
        "pie" => {
            let pie = parse_pie(source)?;
            layout_pie(&pie, columns)
        }
        "classdiagram" => {
            let diagram = parse_class_diagram(source)?;
            layout_class_diagram(&diagram, columns)
        }
        "gantt" => {
            let gantt = parse_gantt(source)?;
            layout_gantt(&gantt, columns)
        }
        _ => None,
    }
}

/// Rewrite state-diagram syntax into flowchart syntax: `a --> b: lbl`
/// becomes `a -- lbl --> b`, `[*]` terminators become `◉` nodes.
/// Composite states and notes are unsupported (`None`).
fn adapt_state_diagram(source: &str) -> Option<String> {
    let mut out = String::from("graph TD\n");
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let keyword = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match keyword.as_str() {
            "statediagram" | "statediagram-v2" => {}
            "direction" => {
                let dir = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_ascii_uppercase();
                if !matches!(dir.as_str(), "TB" | "TD") {
                    return None;
                }
            }
            "state" | "note" | "class" => return None,
            _ => {
                let line = line.strip_suffix(';').unwrap_or(line).trim_end();
                let (edge, label) = match line.split_once(':') {
                    Some((edge, label)) => (edge.trim(), label.trim()),
                    None => (line, ""),
                };
                if label.is_empty() {
                    out.push_str(edge);
                    out.push('\n');
                    continue;
                }
                // Labeled edge: move the target behind the label arrow —
                // `a --> b: lbl` becomes `a -- lbl --> b`.
                match edge.split_once("-->") {
                    Some((left, right)) => {
                        out.push_str(&format!("{} -- {} -->{}\n", left.trim_end(), label, right));
                    }
                    None => return None,
                }
            }
        }
    }
    // Distinct entry/exit terminators: mapping every marker onto one
    // node would fold the entry and exit edges into a layout cycle.
    let out = out.replace("[*] -->", "▶ -->");
    let out = out.replace("--> [*]", "--> ■");
    Some(out)
}

/// A parsed sequence diagram: participant display names and messages
/// `(from, to, label, dotted)` by participant index.
struct Sequence {
    names: Vec<String>,
    messages: Vec<(usize, usize, String, bool)>,
}

/// Parse a sequence diagram; `None` for unsupported constructs
/// (activations, notes, loops) and malformed messages.
fn parse_sequence(source: &str) -> Option<Sequence> {
    let mut names: Vec<String> = Vec::new();
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut messages = Vec::new();
    for raw in source.lines() {
        let line = raw
            .trim()
            .strip_suffix(';')
            .unwrap_or(raw.trim())
            .trim_end();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let keyword = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match keyword.as_str() {
            "sequencediagram" => {}
            "participant" | "actor" => {
                let rest = line[line.find(char::is_whitespace)?..].trim();
                let (id, name) = match rest.split_once(" as ") {
                    Some((id, name)) => (id.trim(), name.trim()),
                    None => (rest, rest),
                };
                if id.is_empty() || name.is_empty() {
                    return None;
                }
                if let Some(&index) = ids.get(id) {
                    // Re-declaration: first name wins; nothing to do.
                    let _ = index;
                } else {
                    ids.insert(id.to_string(), names.len());
                    names.push(name.to_string());
                }
            }
            "auton" | "activate" | "deactivate" | "note" | "loop" | "alt" | "else" | "end"
            | "par" | "critical" | "box" => return None,
            _ => {
                // Message: `A->>B: text`, `A-->>B: text`, `A->B: text`.
                let (heads, label) = line.split_once(':')?;
                let label = label.trim().to_string();
                let (from, to, dotted) = if let Some((from, to)) = heads.split_once("-->>") {
                    (from, to, true)
                } else if let Some((from, to)) = heads.split_once("->>") {
                    (from, to, false)
                } else if let Some((from, to)) = heads.split_once("-->") {
                    (from, to, true)
                } else {
                    let (from, to) = heads.split_once("->")?;
                    (from, to, false)
                };
                let from = *ids.get(from.trim())?;
                let to = *ids.get(to.trim())?;
                messages.push((from, to, label, dotted));
            }
        }
    }
    if names.is_empty() {
        return None;
    }
    Some(Sequence { names, messages })
}

/// Lay a sequence diagram out: participant boxes in a row, one
/// label-plus-arrow row pair per message, lifelines at the boxes.
fn layout_sequence(s: &Sequence, columns: usize) -> Option<Vec<String>> {
    if s.names.len() > 12 {
        return None;
    }
    let gap = 8usize;
    let widths: Vec<usize> = s
        .names
        .iter()
        .map(|name| width::width(name).max(4) + 2)
        .collect();
    let mut centers = Vec::with_capacity(s.names.len());
    let mut cursor = 0usize;
    for (index, w) in widths.iter().enumerate() {
        centers.push(cursor + w / 2);
        cursor += w;
        if index + 1 < widths.len() {
            cursor += gap;
        }
    }
    let total = cursor;
    if total > columns {
        return None;
    }

    let mut lines = Vec::new();
    // Participant boxes.
    let mut top = RowCanvas::new(total);
    let mut mid = RowCanvas::new(total);
    let mut bottom = RowCanvas::new(total);
    let mut offset = 0usize;
    for (index, name) in s.names.iter().enumerate() {
        let w = widths[index];
        top.put(offset, format!("┌{}┐", "─".repeat(w - 2)));
        bottom.put(offset, format!("└{}┘", "─".repeat(w - 2)));
        let label_w = width::width(name);
        let pad = w - 2 - label_w;
        mid.put(
            offset,
            format!(
                "│{}{name}{}│",
                " ".repeat(pad / 2),
                " ".repeat(pad - pad / 2)
            ),
        );
        offset += w + gap;
    }
    lines.push(top.render());
    lines.push(mid.render());
    lines.push(bottom.render());

    // Lifelines skip the span an arrow occupies.
    let lifeline = |row: &mut RowCanvas, skip: Option<(usize, usize)>| {
        for center in &centers {
            let in_span = skip.is_some_and(|(a, b)| *center > a && *center < b);
            if !in_span {
                row.put(*center, "│");
            }
        }
    };
    let mut row = RowCanvas::new(total);
    lifeline(&mut row, None);
    lines.push(row.render());

    for (from, to, label, dotted) in &s.messages {
        let (a, b) = (centers[*from], centers[*to]);
        let (left, right) = if a < b { (a, b) } else { (b, a) };
        // Label row: centered over the span between the two centers.
        let mut label_row = RowCanvas::new(total);
        lifeline(&mut label_row, Some((left, right)));
        if left + 1 < right {
            let label_w = width::width(label);
            let room = right - left - 1;
            if label_w <= room {
                let col = left + 1 + (room - label_w) / 2;
                label_row.put(col, label.clone());
            } else {
                // Label wider than the span: place at the left edge and
                // let the canvas clip what follows.
                label_row.put(left + 1, label.clone());
            }
        } else {
            label_row.put(right + 1, label.clone());
        }
        lines.push(label_row.render());
        // Arrow row.
        let mut arrow_row = RowCanvas::new(total);
        lifeline(&mut arrow_row, Some((left, right)));
        if left == right {
            // Self message: a loop marker instead of a zero-width arrow.
            arrow_row.put(left + 1, "↻");
        } else {
            let fill = if *dotted { "┄" } else { "─" };
            for col in left..right {
                arrow_row.put(col, fill);
            }
            arrow_row.put(right, "▶");
        }
        lines.push(arrow_row.render());
    }
    Some(lines)
}

/// A parsed class diagram: classes with member rows, plus relations
/// `(child, parent)` from `<|--` inheritance arrows.
struct ClassDiagram {
    classes: Vec<(String, Vec<String>)>,
    relations: Vec<(String, String)>,
}

/// Parse a class diagram; unsupported constructs (interfaces,
/// namespaces, annotations) fall back to the source view.
fn parse_class_diagram(source: &str) -> Option<ClassDiagram> {
    let mut classes: Vec<(String, Vec<String>)> = Vec::new();
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut relations = Vec::new();
    for raw in source.lines() {
        let line = raw
            .trim()
            .strip_suffix(';')
            .unwrap_or(raw.trim())
            .trim_end();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let keyword = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match keyword.as_str() {
            "classdiagram" => {}
            "class" => {
                // `class X {` opens a member block; `class X` is bare.
                let rest = line["class".len()..].trim();
                if let Some(name) = rest.strip_suffix('{') {
                    let name = name.trim();
                    ids.entry(name.to_string()).or_insert(classes.len());
                    classes.push((name.to_string(), Vec::new()));
                } else if rest.contains(char::is_whitespace) || rest.contains('(') {
                    return None; // typed members on the class line
                } else {
                    ids.entry(rest.to_string()).or_insert(classes.len());
                    classes.push((rest.to_string(), Vec::new()));
                }
            }
            "interface" | "namespace" | "enumeration" | "note" | "abstract" => return None,
            "}" => {}
            _ => {
                if line.starts_with('+') || line.starts_with('-') || line.starts_with('#') {
                    // A member row of the open class block.
                    let class = classes.last_mut()?;
                    class.1.push(line.to_string());
                } else if let Some((left, right)) = line.split_once("<|--") {
                    relations.push((right.trim().to_string(), left.trim().to_string()));
                } else if line.contains("--") || line.contains("<..") || line.contains("..|>") {
                    return None; // associations beyond inheritance
                } else {
                    return None;
                }
            }
        }
    }
    if classes.is_empty() {
        return None;
    }
    Some(ClassDiagram { classes, relations })
}

/// Lay a class diagram out: one member box per class, side by side,
/// inheritance rows under the boxes (`child ──▷ parent`).
fn layout_class_diagram(d: &ClassDiagram, columns: usize) -> Option<Vec<String>> {
    if d.classes.len() > 8 {
        return None;
    }
    let mut widths = Vec::with_capacity(d.classes.len());
    for (name, members) in &d.classes {
        let member_w = members.iter().map(|m| width::width(m)).max().unwrap_or(0);
        // The name row draws `│name│`: the box must fit the name plus
        // both bars, or a long name bleeds into the neighbor box.
        widths.push(member_w.max(8).max(width::width(name) + 2));
    }
    let gap = 3usize;
    let total: usize = widths.iter().sum::<usize>() + gap * d.classes.len().saturating_sub(1);
    if total > columns {
        return None;
    }
    let mut lines = Vec::new();
    let mut offset = 0usize;
    let mut centers = Vec::with_capacity(d.classes.len());
    let mut name_row = RowCanvas::new(total);
    for (index, (name, _)) in d.classes.iter().enumerate() {
        let w = widths[index];
        let pad = w.saturating_sub(width::width(name));
        name_row.put(
            offset,
            format!(
                "\u{2502}{}{name}{}\u{2502}",
                " ".repeat(pad / 2),
                " ".repeat(pad - pad / 2)
            ),
        );
        centers.push(offset + w / 2);
        offset += w + gap;
    }
    lines.push(name_row.render());
    let body_rows = d.classes.iter().map(|(_, m)| m.len()).max().unwrap_or(0);
    for row in 0..body_rows {
        let mut canvas = RowCanvas::new(total);
        let mut offset = 0usize;
        for (index, (_, members)) in d.classes.iter().enumerate() {
            let w = widths[index];
            match members.get(row) {
                Some(member) => canvas.put(offset, format!("\u{2502}{member}")),
                None => canvas.put(offset, "\u{2502}"),
            }
            offset += w + gap;
        }
        lines.push(canvas.render());
    }
    // Box bottoms: a bottom border per box.
    let mut bottom = RowCanvas::new(total);
    let mut offset = 0usize;
    for w in &widths {
        bottom.put(
            offset,
            format!("\u{2534}{}\u{2534}", "\u{2500}".repeat(w - 2)),
        );
        offset += w + gap;
    }
    lines.push(bottom.render());
    // Inheritance rows.
    for (child, parent) in &d.relations {
        lines.push(format!("{child} \u{2500}\u{2500}\u{25b7} {parent}"));
    }
    Some(lines)
}

/// A parsed gantt chart: title plus `(name, start_day, days)` tasks in
/// declaration order.
struct Gantt {
    title: Option<String>,
    tasks: Vec<(String, i64, i64)>,
}

/// Parse a gantt chart. Only `YYYY-MM-DD` dates and day durations
/// (`5d`) are supported; `after x` resolves through earlier tasks.
/// Anything else (unnamed formats, milestones, excludes) falls back.
fn parse_gantt(source: &str) -> Option<Gantt> {
    let mut title = None;
    let mut tasks: Vec<(String, i64, i64)> = Vec::new();
    let mut starts: HashMap<String, i64> = HashMap::new();
    for raw in source.lines() {
        let line = raw
            .trim()
            .strip_suffix(';')
            .unwrap_or(raw.trim())
            .trim_end();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "gantt") {
            continue;
        }
        if starts_with_keyword(line, "title") {
            title = Some(line["title".len()..].trim().to_string());
            continue;
        }
        if starts_with_keyword(line, "dateformat")
            || starts_with_keyword(line, "section")
            || starts_with_keyword(line, "excludes")
        {
            continue;
        }
        // Task: `name :[id,] (start|after x), Nd[, tag...]`. The id is
        // optional and the trailing tag (`done`, `active`, ...) is
        // ignored; an id-less task is referenced by its name.
        let (name, spec) = line.split_once(':')?;
        let name = name.trim().to_string();
        let parts: Vec<&str> = spec.split(',').map(str::trim).collect();
        // A leading date or `after` clause means the id was omitted.
        let (id, rest): (&str, &[&str]) = match parts.split_first() {
            Some((first, _)) if is_start_spec(first) => ("", parts.as_slice()),
            Some((first, rest)) => (first, rest),
            None => return None,
        };
        let start_spec = rest.first().copied()?;
        let duration: i64 = rest
            .get(1)
            .and_then(|part| part.strip_suffix('d'))
            .and_then(|days| days.parse().ok())?;
        if !(0..=MAX_GANTT_DAYS).contains(&duration) {
            return None;
        }
        let start = if let Some(dep) = start_spec.strip_prefix("after ") {
            *starts.get(dep.trim())?
        } else {
            days_from_civil(start_spec)?
        };
        if !id.is_empty() {
            starts.insert(id.to_string(), start);
        } else {
            starts.insert(name.clone(), start);
        }
        tasks.push((name, start, duration));
    }
    if tasks.is_empty() {
        return None;
    }
    Some(Gantt { title, tasks })
}

/// True when the part is a start spec (a `YYYY-MM-DD` date or an
/// `after id` clause) rather than a task id.
fn is_start_spec(part: &str) -> bool {
    part.starts_with("after ") || days_from_civil(part).is_some()
}

/// True when `line` starts with the case-insensitive directive `kw`
/// followed by whitespace or end-of-line: task and slice lines whose
/// first word merely extends the keyword stay data.
fn starts_with_keyword(line: &str, kw: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    match lower.strip_prefix(kw) {
        Some(rest) => rest.is_empty() || rest.starts_with(char::is_whitespace),
        None => false,
    }
}

/// Upper bound on a task duration in days (ten thousand years): keeps
/// `start + days` and the inverse civil-date arithmetic inside i64.
const MAX_GANTT_DAYS: i64 = 3_652_059;

/// Days since 1970-01-01 for a `YYYY-MM-DD` date (Howard Hinnant's
/// days_from_civil, proleptic Gregorian).
fn days_from_civil(date: &str) -> Option<i64> {
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    // Gregorian wall-clock dates only: the year bound keeps the day
    // arithmetic (and `civil_from_days` of `start + days`) inside i64.
    if !(1..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// Render a gantt chart: title row plus one bar row per task. Bars are
/// scaled against the longest task; dates render MM-DD.
fn layout_gantt(g: &Gantt, columns: usize) -> Option<Vec<String>> {
    let max_name = g
        .tasks
        .iter()
        .map(|(name, _, _)| width::width(name))
        .max()?
        .max(4);
    let bar_max = 24usize;
    let max_days = g.tasks.iter().map(|(_, _, d)| *d).max()?;
    if max_days <= 0 || max_name + bar_max + 18 > columns {
        return None;
    }
    let mut lines = Vec::new();
    if let Some(title) = &g.title {
        lines.push(title.clone());
    }
    for (name, start, days) in &g.tasks {
        let bar_len = ((*days as f64 / max_days as f64) * bar_max as f64).round() as usize;
        let bar = "\u{2588}".repeat(bar_len.max(1));
        let end = civil_from_days(start + days);
        lines.push(format!("{name:<max_name$} {bar} {}", &end[5..],));
    }
    Some(lines)
}

/// Inverse of [`days_from_civil`] (Hinnant's civil_from_days).
fn civil_from_days(z: i64) -> String {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// A parsed pie chart: title plus `(label, value)` slices.
struct Pie {
    title: Option<String>,
    slices: Vec<(String, f64)>,
}

/// Parse a pie chart; `None` on malformed slices.
fn parse_pie(source: &str) -> Option<Pie> {
    let mut title = None;
    let mut slices = Vec::new();
    for raw in source.lines() {
        let line = raw
            .trim()
            .strip_suffix(';')
            .unwrap_or(raw.trim())
            .trim_end();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if starts_with_keyword(line, "pie") {
            // `pie title X` or a bare `pie`.
            let rest = line["pie".len()..].trim();
            if let Some(t) = rest.strip_prefix("title") {
                title = Some(t.trim().to_string());
            }
            continue;
        }
        if lower == "showdata" {
            continue;
        }
        let (label, value) = line.split_once(':')?;
        let label = label.trim().trim_matches('"').to_string();
        let value: f64 = value.trim().parse().ok()?;
        slices.push((label, value));
    }
    if slices.is_empty() {
        return None;
    }
    Some(Pie { title, slices })
}

/// Render a pie chart as a horizontal bar chart with percentages —
/// the honest terminal encoding of a pie.
fn layout_pie(pie: &Pie, columns: usize) -> Option<Vec<String>> {
    let max_label = pie
        .slices
        .iter()
        .map(|(label, _)| width::width(label))
        .max()?
        .max(4);
    let bar_max = 20usize;
    let total: f64 = pie.slices.iter().map(|(_, value)| value).sum();
    // A poisoned total (NaN, or non-positive after negative slices)
    // falls back to the source view instead of rendering `NaN%`.
    if !total.is_finite() || total <= 0.0 {
        return None;
    }
    if max_label + 1 + bar_max + 8 > columns {
        return None;
    }
    let mut lines = Vec::new();
    if let Some(title) = &pie.title {
        lines.push(title.clone());
    }
    for (label, value) in &pie.slices {
        let share = (value / total * 100.0).clamp(0.0, 100.0);
        let bar_len = ((value / total) * bar_max as f64).round() as usize;
        let bar = "█".repeat(bar_len);
        let label_col = max_label - width::width(label);
        lines.push(format!(
            "{}{} {bar} {:>5.1}%",
            label,
            " ".repeat(label_col),
            share
        ));
    }
    Some(lines)
}

/// A parsed flowchart: node labels in definition order plus edges
/// between node indices.
struct Diagram {
    labels: Vec<String>,
    edges: Vec<(usize, usize, Option<String>)>,
}

/// Parse a flowchart source; `None` for unsupported diagrams.
fn parse(source: &str) -> Option<Diagram> {
    let mut labels: Vec<String> = Vec::new();
    let mut ids: HashMap<String, usize> = HashMap::new();
    let mut edges: Vec<(usize, usize, Option<String>)> = Vec::new();
    let mut direction_seen = false;
    for raw in source.lines() {
        let line = raw
            .trim()
            .strip_suffix(';')
            .unwrap_or(raw.trim())
            .trim_end();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let keyword = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match keyword.as_str() {
            "graph" | "flowchart" => {
                let dir = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_ascii_uppercase();
                match dir.as_str() {
                    "TD" | "TB" => direction_seen = true,
                    _ => return None,
                }
            }
            // Styling directives ride along ignored; subgraphs and any
            // other diagram kind fall back to the source view.
            "subgraph" => return None,
            "end" => return None,
            "classdef" | "class" | "style" | "linkstyle" | "click" => {}
            _ => parse_statement(line, &mut labels, &mut ids, &mut edges)?,
        }
    }
    if !direction_seen || labels.is_empty() {
        return None;
    }
    Some(Diagram { labels, edges })
}

/// Parse one statement line: an edge chain or a bare node definition.
fn parse_statement(
    line: &str,
    labels: &mut Vec<String>,
    ids: &mut HashMap<String, usize>,
    edges: &mut Vec<(usize, usize, Option<String>)>,
) -> Option<()> {
    let (left, consumed) = scan_node_token(line)?;
    let from = node_index(labels, ids, left)?;
    let rest = line[consumed..].trim();
    if rest.is_empty() {
        return Some(()); // bare node definition
    }
    parse_edges(from, rest, labels, ids, edges)
}

/// Parse an edge chain iteratively: each pass consumes one
/// `<arrow> <target>` pair and continues from its tail, so chains of
/// any length cannot exhaust the stack.
fn parse_edges(
    mut from: usize,
    mut rest: &str,
    labels: &mut Vec<String>,
    ids: &mut HashMap<String, usize>,
    edges: &mut Vec<(usize, usize, Option<String>)>,
) -> Option<()> {
    loop {
        let (label, consumed) = scan_arrow(rest)?;
        let after = rest[consumed..].trim_start();
        let (label, after) = pipe_label(after, label);
        let (target, tail_consumed) = scan_node_token(after)?;
        let to = node_index(labels, ids, target)?;
        edges.push((from, to, label));
        let tail = after[tail_consumed..].trim();
        if tail.is_empty() {
            return Some(());
        }
        from = to;
        rest = tail;
    }
}

/// Split a leading node token off `line`: the token ends at the first
/// top-level edge start — `-->`, `---`, `-.`, `==>` — so a labeled
/// arrow (`B -- 是 --> C`) cuts at the opening `--` instead of
/// swallowing the label into the node token.
fn scan_node_token(line: &str) -> Option<(&str, usize)> {
    let bytes = line.as_bytes();
    let mut depth = 0i32;
    for (index, byte) in bytes.iter().enumerate() {
        match *byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'-' | b'=' if depth == 0 => {
                let rest = &line[index..];
                let edge =
                    rest.starts_with("--") || rest.starts_with("-.") || rest.starts_with("==");
                if edge {
                    let token = line[..index].trim_end();
                    if token.is_empty() {
                        return None;
                    }
                    return Some((token, index));
                }
            }
            _ => {}
        }
    }
    let token = line.trim_end();
    (!token.is_empty()).then_some((token, line.len()))
}

/// Match one arrow (with optional inline label) at the start of `line`.
/// Returns the label and the consumed width.
fn scan_arrow(line: &str) -> Option<(Option<String>, usize)> {
    let dotted = line.starts_with("-.");
    let thick = !dotted && line.starts_with("==");
    let plain = !dotted && !thick && line.starts_with("--");
    if dotted {
        if line.starts_with("-.->") {
            return Some((None, 4));
        }
        let end = line.find(".->")?;
        let label = line[2..end].trim();
        let label = (!label.is_empty()).then(|| label.to_string());
        return Some((label, end + 3));
    }
    if thick {
        if line.starts_with("==>") {
            return Some((None, 3));
        }
        let end = line.find("==>")?;
        let label = line[2..end].trim();
        let label = (!label.is_empty()).then(|| label.to_string());
        return Some((label, end + 3));
    }
    if plain {
        if line.starts_with("-->") {
            return Some((None, 3));
        }
        if line.starts_with("---") {
            return Some((None, 3));
        }
        let end = line.find("-->")?;
        let label = line[2..end].trim();
        let label = (!label.is_empty()).then(|| label.to_string());
        return Some((label, end + 3));
    }
    None
}

/// Absorb a `-->|label|` suffix: when `after` opens with a pipe, the
/// label between the pipes wins over the inline form.
fn pipe_label(after: &str, label: Option<String>) -> (Option<String>, &str) {
    let Some(rest) = after.strip_prefix('|') else {
        return (label, after);
    };
    match rest.find('|') {
        Some(end) => {
            let inner = rest[..end].trim();
            let label = (!inner.is_empty()).then(|| inner.to_string());
            (label, &rest[end + 1..])
        }
        None => (label, after),
    }
}

/// Register a node token (id plus optional shape), returning its index.
fn node_index(
    labels: &mut Vec<String>,
    ids: &mut HashMap<String, usize>,
    token: &str,
) -> Option<usize> {
    let (id, label) = node_token(token)?;
    if let Some(&index) = ids.get(&id) {
        return Some(index);
    }
    let index = labels.len();
    labels.push(label);
    ids.insert(id, index);
    Some(index)
}

/// Split a node token into (id, label): `A[开始]` keeps id `A` and
/// label `开始`, the doubled shapes (`A[[仓库]]`, `A((开始))`) peel
/// both layers, and a bare id is its own label. An unclosed shape
/// (`A[开始`, a streaming prefix) parses with its text as the label;
/// nothing left inside (`A(`) holds no label.
fn node_token(token: &str) -> Option<(String, String)> {
    let token = token.trim();
    // The openers are ASCII, so `find` always lands on a char boundary.
    let id_end = token.find(['(', '[', '{']).unwrap_or(token.len());
    let id = token[..id_end].trim();
    if id.is_empty() {
        return None;
    }
    if id_end == token.len() {
        // A bare id is its own label.
        return Some((id.to_string(), id.to_string()));
    }
    let opener = token.as_bytes()[id_end];
    let closer = match opener {
        b'(' => ')',
        b'[' => ']',
        _ => '}',
    };
    if id_end + 1 == token.len() {
        // `A(`, `A[`: an opener with nothing inside holds no label.
        return None;
    }
    // The opener byte is consumed; an unclosed shape (a streaming
    // prefix like `A[开始`) keeps its text as the label.
    let mut label = &token[id_end + 1..];
    if label.ends_with(closer) {
        label = &label[..label.len() - 1];
    }
    // Doubled shapes (`A[[仓库]]`, `A((开始))`) peel the inner layer.
    if label.len() >= 2 && label.starts_with(opener as char) && label.ends_with(closer) {
        label = &label[1..label.len() - 1];
    }
    let label = label.trim();
    if label.is_empty() {
        None
    } else {
        Some((id.to_string(), label.to_string()))
    }
}

/// One display row: cells placed at exact display columns, last write
/// wins. Renders with single-width spaces in the gaps.
struct RowCanvas {
    placements: Vec<(usize, String)>,
    width: usize,
}

impl RowCanvas {
    fn new(width: usize) -> Self {
        Self {
            placements: Vec::new(),
            width,
        }
    }

    /// Place `text` at display column `col`, dropping overlapped cells.
    fn put(&mut self, col: usize, text: impl Into<String>) {
        let text = text.into();
        let span = width::width(&text);
        if span == 0 || col >= self.width {
            return;
        }
        let end = col + span;
        self.placements
            .retain(|(c, t)| c + width::width(t) <= col || *c >= end);
        self.placements.push((col, text));
    }

    /// Render the row: placements sorted by column, spaces elsewhere.
    fn render(&self) -> String {
        let mut items = self.placements.clone();
        items.sort_by_key(|(col, _)| *col);
        let mut out = String::new();
        let mut pos = 0usize;
        for (col, text) in items {
            if col < pos {
                continue; // safety: overlap filter already removed these
            }
            out.push_str(&" ".repeat(col - pos));
            pos = col + width::width(&text);
            out.push_str(&text);
        }
        if pos < self.width {
            out.push_str(&" ".repeat(self.width - pos));
        }
        out
    }
}

/// Lay the diagram out into band rows; `None` when it cannot fit
/// `columns` (the caller falls back to the source view).
fn layout(d: &Diagram, columns: usize) -> Option<Vec<String>> {
    let count = d.labels.len();
    if count > 100 {
        return None;
    }
    // Longest-path layering by relaxation. A cycle makes the layers
    // grow by one per pass without ever settling: detect it by capping
    // every layer at the node count and fall back to the source view —
    // a cyclic flow (state loops, recursive calls) has no faithful
    // top-down layout in this model.
    let mut layer = vec![0usize; count];
    for _ in 0..count {
        for (from, to, _) in &d.edges {
            if layer[*to] < layer[*from] + 1 {
                layer[*to] = layer[*from] + 1;
                if layer[*to] >= count {
                    return None; // cycle
                }
            }
        }
    }
    let max_layer = layer.iter().copied().max().unwrap_or(0);
    let mut bands: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for (index, depth) in layer.iter().enumerate() {
        bands[*depth].push(index);
    }

    // Box geometry: sequential placement inside each band, 3-space
    // gutters between boxes, shared across bands.
    let mut x = vec![0usize; count];
    let mut box_w = vec![0usize; count];
    let mut total_width = 0usize;
    for band in &bands {
        let mut cursor = 0usize;
        for &index in band {
            let label_w = width::width(&d.labels[index]).max(2);
            box_w[index] = label_w + 4; // borders plus one space of padding
            x[index] = cursor;
            cursor += box_w[index] + 3;
        }
        total_width = total_width.max(cursor.saturating_sub(3));
    }
    if total_width > columns {
        return None;
    }

    // Gap heights: one elbow row per terminating edge plus a vertical
    // row and the arrowhead row.
    let mut gap_height = vec![2usize; max_layer];
    let mut terminating = vec![0usize; max_layer];
    for (from, to, _) in &d.edges {
        if layer[*to] > layer[*from] {
            terminating[layer[*to] - 1] += 1;
        }
    }
    for (gap, count) in terminating.iter().enumerate() {
        gap_height[gap] = gap_height[gap].max(count + 1);
    }

    // Row offset of each band top.
    let mut band_top = vec![0usize; max_layer + 1];
    let mut row = 0usize;
    for depth in 0..=max_layer {
        band_top[depth] = row;
        row += 3;
        if depth < max_layer {
            row += gap_height[depth];
        }
    }

    let mut gap_rows: Vec<Vec<RowCanvas>> = gap_height
        .iter()
        .map(|height| (0..*height).map(|_| RowCanvas::new(total_width)).collect())
        .collect();
    let mut elbow_slot = vec![0usize; max_layer];
    for (from, to, label) in &d.edges {
        let (fl, tl) = (layer[*from], layer[*to]);
        if tl == fl {
            continue; // same-layer edges do not route (v1)
        }
        let from_center = x[*from] + box_w[*from] / 2;
        let to_center = x[*to] + box_w[*to] / 2;
        for gap in fl..tl {
            let top = band_top[gap] + 3;
            let height = gap_height[gap];
            let rows = &mut gap_rows[gap];
            if gap + 1 < tl {
                // Pass-through: a straight vertical in every gap row.
                for canvas in rows.iter_mut() {
                    canvas.put(from_center, "│");
                }
                continue;
            }
            if from_center == to_center {
                for canvas in rows.iter_mut().take(height - 1) {
                    canvas.put(from_center, "│");
                }
                rows[height - 1].put(to_center, "▼");
                continue;
            }
            let slot = elbow_slot[gap];
            elbow_slot[gap] += 1;
            let elbow = top + slot.min(height.saturating_sub(2));
            for canvas in rows.iter_mut().take(elbow - top) {
                canvas.put(from_center, "│");
            }
            let (left, right) = if from_center < to_center {
                (from_center, to_center)
            } else {
                (to_center, from_center)
            };
            rows[elbow - top].put(from_center, "└");
            rows[elbow - top].put(to_center, "┘");
            if !draw_horizontal(&mut rows[elbow - top], left + 1, right, label.as_deref())
                && let Some(text) = label
            {
                // Span too narrow for the label: annotate beside the
                // arrowhead instead of dropping it.
                rows[height - 2].put(right + 2, text.clone());
            }
            for canvas in rows.iter_mut().take(top + height - 1).skip(elbow + 1 - top) {
                canvas.put(to_center, "│");
            }
            rows[height - 1].put(to_center, "▼");
        }
    }

    let mut lines: Vec<String> = Vec::with_capacity(row);
    for depth in 0..=max_layer {
        let mut top_row = RowCanvas::new(total_width);
        let mut mid_row = RowCanvas::new(total_width);
        let mut bottom_row = RowCanvas::new(total_width);
        for &index in &bands[depth] {
            let (bx, bw) = (x[index], box_w[index]);
            top_row.put(bx, format!("┌{}┐", "─".repeat(bw - 2)));
            bottom_row.put(bx, format!("└{}┘", "─".repeat(bw - 2)));
            let label = &d.labels[index];
            let label_w = width::width(label).max(2);
            let pad = bw - 2 - label_w;
            let left = pad / 2;
            mid_row.put(
                bx,
                format!("│{}{label}{}│", " ".repeat(left), " ".repeat(pad - left)),
            );
        }
        lines.push(top_row.render());
        lines.push(mid_row.render());
        lines.push(bottom_row.render());
        if depth < max_layer {
            for canvas in &gap_rows[depth] {
                lines.push(canvas.render());
            }
        }
    }
    Some(lines)
}

/// Fill a horizontal run with dashes, swapping the middle for an edge
/// label when one fits. Runs either direction; an overlapping vertical
/// at the run's own column is kept (the corner glyph wins).
fn draw_horizontal(row: &mut RowCanvas, start: usize, end: usize, label: Option<&str>) -> bool {
    if end <= start {
        return false;
    }
    for col in start..end {
        row.put(col, "─");
    }
    if let Some(label) = label {
        let label_w = width::width(label);
        let room = end - start;
        if label_w < room {
            let col = start + (room - label_w) / 2;
            row.put(col, label.to_string());
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that flip the global render toggle.
    static TOGGLE_LOCK: Mutex<()> = Mutex::new(());

    const FLOW: &str = "\
graph TD
    A[开始] --> B{是否?}
    B -- 是 --> C[执行]
    B -- 否 --> D[跳过]
    C --> E[结束]
    D --> E";

    #[test]
    fn flowchart_renders_bands_and_arrows() {
        let lines = render_diagram(FLOW, 80).expect("supported flowchart renders");
        let joined = lines.join("\n");
        // Every node label appears exactly once inside a box row.
        for label in ["开始", "是否?", "执行", "跳过", "结束"] {
            assert!(joined.contains(label), "label {label}: {joined}");
        }
        // Boxes carry all three borders.
        assert!(lines.iter().any(|l| l.contains('┌') && l.contains('┐')));
        assert!(lines.iter().any(|l| l.contains('└') && l.contains('┘')));
        // Edge labels ride the elbow rows.
        assert!(joined.contains('是'), "edge label 是: {joined}");
        assert!(joined.contains('否'), "edge label 否: {joined}");
        // Arrowheads point into the next band.
        assert!(joined.contains('▼'), "arrowheads: {joined}");
        // Every line fits the budget.
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn chains_and_pipe_labels_parse() {
        let lines =
            render_diagram("graph TD\n a --> b --> c\n b -->|yes| d", 80).expect("chain renders");
        let joined = lines.join("\n");
        for label in ["a", "b", "c", "d"] {
            assert!(joined.contains(label), "node {label}: {joined}");
        }
    }

    #[test]
    fn unsupported_shapes_fall_back_to_none() {
        assert!(render_diagram("graph LR\n a --> b", 80).is_none(), "LR");
        assert!(render_diagram("sequenceDiagram\n a->>b", 80).is_none());
        assert!(
            render_diagram("graph TD\n subgraph s\n a --> b\n end", 80).is_none(),
            "subgraph"
        );
        assert!(render_diagram("a --> b", 80).is_none(), "no direction line");
    }

    /// A streaming fence delivers prefixes before any closing bracket:
    /// an unclosed shape with a multibyte label (`A[开始`) must parse
    /// instead of panicking on a mid-glyph byte slice.
    #[test]
    fn unclosed_shapes_with_multibyte_labels_do_not_panic() {
        assert!(render_diagram("graph TD\n    A[开始\n    B --> C", 80).is_some());
        assert!(render_diagram("graph TD\n    A{循环判断", 80).is_some());
        // Empty unclosed shapes hold no label and stay unsupported.
        assert!(render_diagram("graph TD\n    A(\n    B --> C", 80).is_none());
    }

    /// Every supported shape peels to its bare label, doubled layers
    /// (`[[..]]`, `((..))`) included.
    #[test]
    fn shape_labels_peel_all_bracket_layers() {
        for (token, label) in [
            ("A[text]", "text"),
            ("A(圆)", "圆"),
            ("A{菱形}", "菱形"),
            ("A[[仓库]]", "仓库"),
            ("A((开始))", "开始"),
            ("A[开 始]", "开 始"),
        ] {
            let src = format!("graph TD\n {token} --> B");
            let lines = render_diagram(&src, 80).expect("shape renders");
            let joined = lines.join("\n");
            assert!(joined.contains(label), "{token} -> {label}: {joined}");
        }
    }

    /// Edge chains parse iteratively: a chain far beyond any stack
    /// budget must not overflow the stack.
    #[test]
    fn long_edge_chains_parse_without_stack_overflow() {
        let mut chain = String::from("graph TD\n n0");
        for i in 0..50_000 {
            chain.push_str(&format!(" --> n{}", i + 1));
        }
        // Too wide to lay out at 80 columns, but the parse must hold.
        assert!(render_diagram(&chain, 80).is_none());
    }

    #[test]
    fn oversized_diagrams_fall_back_to_none() {
        assert!(render_diagram(FLOW, 10).is_none(), "too narrow: fallback");
    }

    /// The markdown hook only renders when the global toggle is on.
    #[test]
    fn state_diagrams_render_as_flowcharts() {
        let src = "stateDiagram-v2\n    [*] --> \u{5f85}\u{5904}\u{7406}\n    \u{5f85}\u{5904}\u{7406} --> \u{5b8c}\u{6210}: start\n    \u{5b8c}\u{6210} --> [*]";
        let lines = render_diagram(src, 80).expect("state diagram renders");
        let joined = lines.join("\n");
        assert!(joined.contains("\u{5f85}\u{5904}\u{7406}"), "{joined}");
        assert!(joined.contains("\u{25b6}"), "entry terminator: {joined}");
        assert!(joined.contains("\u{25a0}"), "exit terminator: {joined}");
        assert!(joined.contains("start"), "edge label: {joined}");
    }

    #[test]
    fn sequence_diagrams_render_participants_and_arrows() {
        let src = "sequenceDiagram\n    participant U as \u{7528}\u{6237}\n    participant A as Agent\n    U->>A: hello\n    A-->>U: hi";
        let lines = render_diagram(src, 80).expect("sequence diagram renders");
        let joined = lines.join("\n");
        for label in ["hello", "hi", "Agent"] {
            assert!(joined.contains(label), "{label}: {joined}");
        }
        assert!(joined.contains("\u{25b6}"), "arrowheads: {joined}");
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn pie_charts_render_as_labeled_bars() {
        let src = "pie title langs\n    \"JS\" : 35\n    \"Go\" : 15";
        let lines = render_diagram(src, 80).expect("pie renders");
        let joined = lines.join("\n");
        assert!(joined.contains("langs"), "title: {joined}");
        assert!(joined.contains("JS") && joined.contains("Go"), "{joined}");
        assert!(joined.contains('%'), "percentage: {joined}");
    }

    #[test]
    fn class_diagrams_render_boxes_and_inheritance() {
        let src = "classDiagram\n    class Animal {\n        +String name\n        +makeSound()\n    }\n    class Dog\n    Dog <|-- Animal";
        let lines = render_diagram(src, 80).expect("class diagram renders");
        let joined = lines.join("\n");
        assert!(joined.contains("Animal"), "{joined}");
        assert!(joined.contains("makeSound()"), "{joined}");
        assert!(joined.contains("\u{25b7}"), "inheritance arrow: {joined}");
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    /// A class name wider than its members sizes the box: a long name
    /// must not bleed across the gutter and erase the neighbor box.
    #[test]
    fn class_boxes_grow_to_fit_their_names() {
        let src =
            "classDiagram\n    class VeryLongClassName {\n        +run()\n    }\n    class Short";
        let lines = render_diagram(src, 80).expect("class diagram renders");
        let name_row = lines
            .iter()
            .find(|l| l.contains("VeryLongClassName"))
            .expect("name row");
        assert!(
            name_row.contains("Short"),
            "neighbor box survives the long name: {name_row:?}"
        );
        assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
    }

    #[test]
    fn gantt_charts_render_scaled_bars() {
        let src = "gantt\n    title plan\n    dateFormat YYYY-MM-DD\n    section s\n    任务一 :a1, 2026-01-01, 10d\n    任务二 :a2, after a1, 5d";
        let lines = render_diagram(src, 80).expect("gantt renders");
        let joined = lines.join("\n");
        assert!(joined.contains("plan"), "title: {joined}");
        for name in ["任务一", "任务二"] {
            assert!(joined.contains(name), "{name}: {joined}");
        }
        // The 10-day task gets a longer bar than the 5-day one.
        let bar_of = |name: &str| {
            lines
                .iter()
                .find(|l| l.contains(name))
                .map(|l| l.matches('\u{2588}').count())
                .unwrap()
        };
        assert!(bar_of("任务一") > bar_of("任务二"), "{lines:?}");
    }

    /// Real-world gantt shapes: a task with a trailing tag (`done`) and
    /// a task with the id omitted must both render, not fall back.
    #[test]
    fn gantt_tasks_with_tags_and_without_ids_render() {
        let tagged =
            "gantt\n    任务一 :a1, 2026-01-01, 10d, done\n    任务二 :a2, after a1, 5d, active";
        let lines = render_diagram(tagged, 80).expect("tagged tasks render");
        let joined = lines.join("\n");
        assert!(
            joined.contains("任务一") && joined.contains("任务二"),
            "{joined}"
        );
        let idless = "gantt\n    任务一 :2026-01-01, 10d\n    任务二 :after 任务一, 5d";
        let lines = render_diagram(idless, 80).expect("id-less tasks render");
        let joined = lines.join("\n");
        assert!(
            joined.contains("任务一") && joined.contains("任务二"),
            "{joined}"
        );
    }

    /// Leap-year dates round-trip: 2024-02-29 plus one day ends 03-01.
    #[test]
    fn gantt_leap_year_dates_render() {
        let src = "gantt\n    跳日 :a1, 2024-02-29, 1d";
        let lines = render_diagram(src, 80).expect("leap date renders");
        let joined = lines.join("\n");
        assert!(joined.contains("03-01"), "end after the leap day: {joined}");
    }

    /// Absurd dates and durations fall back to the source view instead
    /// of overflowing the day arithmetic.
    #[test]
    fn gantt_out_of_range_values_fall_back_to_none() {
        let huge_year = "gantt\n    t :a, 9223372036854775807-01-01, 5d";
        assert!(render_diagram(huge_year, 80).is_none(), "huge year");
        let huge_span = "gantt\n    t :a, 2026-01-01, 9223372036854775807d";
        assert!(render_diagram(huge_span, 80).is_none(), "huge duration");
        let negative = "gantt\n    t :a, 2026-01-01, -5d";
        assert!(render_diagram(negative, 80).is_none(), "negative duration");
    }

    /// A pie slice whose label merely starts with the word `pie` is
    /// data, not the diagram header.
    #[test]
    fn pie_slices_named_like_the_header_still_render() {
        let src = "pie\n    piece : 5\n    other : 5";
        let lines = render_diagram(src, 80).expect("pie renders");
        let joined = lines.join("\n");
        assert!(joined.contains("piece"), "slice kept: {joined}");
        assert!(joined.contains("other"), "{joined}");
        assert!(joined.contains("50.0%"), "share: {joined}");
    }

    /// A NaN slice value poisons the total: fall back instead of
    /// rendering `NaN%`.
    #[test]
    fn pie_nan_values_fall_back_to_none() {
        let src = "pie\n    a : NaN\n    b : 5";
        assert!(render_diagram(src, 80).is_none(), "NaN total falls back");
    }

    #[test]
    fn toggle_switches_render_mode() {
        let _guard = TOGGLE_LOCK.lock().unwrap();
        let previous = render_enabled();
        set_render_enabled(!previous);
        assert_eq!(render_enabled(), !previous);
        set_render_enabled(previous);
        assert_eq!(render_enabled(), previous);
    }
}
