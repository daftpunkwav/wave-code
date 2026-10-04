//! Row-chart kinds: gitGraph, journey, quadrantChart, xychart-beta,
//! gantt, and pie. Each parses its own source and lays out one row
//! per datum with small dedicated geometry.

use std::collections::HashMap;

use super::layout::RowCanvas;
use super::starts_with_keyword;
use super::truncate_width;
use crate::width;

// ---------------------------------------------------------------------------
// gitGraph — a branch-labeled commit timeline
// ---------------------------------------------------------------------------

/// One commit/merge event on a branch.
pub(super) struct GitEvent {
    branch: String,
    merge: bool,
    label: String,
}

/// Parse a gitGraph source: a flat event list with the current branch
/// tracked through `branch`/`checkout`/`switch`.
pub(super) fn parse_git_graph(source: &str) -> Option<Vec<GitEvent>> {
    let mut events = Vec::new();
    let mut current = "main".to_string();
    let mut seq = 0usize;
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let (keyword, rest) = match line.split_once(char::is_whitespace) {
            Some((kw, rest)) => (kw.to_ascii_lowercase(), rest.trim()),
            None => (line.to_ascii_lowercase(), ""),
        };
        match keyword.as_str() {
            "gitgraph" => {}
            "commit" => {
                seq += 1;
                let id = git_attr(rest, "id").unwrap_or(format!("#{seq}"));
                let label = match git_attr(rest, "tag") {
                    Some(tag) => format!("{id} ({tag})"),
                    None => id,
                };
                events.push(GitEvent {
                    branch: current.clone(),
                    merge: false,
                    label,
                });
            }
            "cherry-pick" => {
                seq += 1;
                let id = git_attr(rest, "id").unwrap_or(format!("#{seq}"));
                events.push(GitEvent {
                    branch: current.clone(),
                    merge: false,
                    label: format!("{id} (cherry)"),
                });
            }
            "branch" => {
                let name = rest.split_whitespace().next()?;
                if name.is_empty() {
                    return None;
                }
                current = name.trim_matches('"').to_string();
            }
            "checkout" | "switch" => {
                let name = rest.split_whitespace().next()?;
                current = name.trim_matches('"').to_string();
            }
            "merge" => {
                let name = rest.split_whitespace().next()?;
                let mut label = format!("merge {}", name.trim_matches('"'));
                if let Some(tag) = git_attr(rest, "tag") {
                    label.push_str(&format!(" ({tag})"));
                }
                events.push(GitEvent {
                    branch: current.clone(),
                    merge: true,
                    label,
                });
            }
            _ => return None,
        }
    }
    if events.is_empty() || events.len() > 80 {
        return None;
    }
    Some(events)
}

/// Lay a gitGraph out: one row per event, the branch lane labeled on
/// the left, commits as `○` and merges as `●`.
pub(super) fn layout_git_graph(events: &[GitEvent], columns: usize) -> Option<Vec<String>> {
    let mut branches: Vec<String> = Vec::new();
    for event in events {
        if !branches.contains(&event.branch) {
            branches.push(event.branch.clone());
        }
    }
    if branches.len() > 8 {
        return None;
    }
    let lane = branches.iter().map(|b| width::width(b)).max()?.max(4);
    let label_w = events.iter().map(|e| width::width(&e.label)).max()?.max(4);
    if lane + 3 + label_w > columns {
        return None;
    }
    let mut lines = Vec::with_capacity(events.len() + 1);
    lines.push(branches.join(" · "));
    for event in events {
        let glyph = if event.merge { "●" } else { "○" };
        let branch_w = width::width(&event.branch);
        lines.push(format!(
            "{}{}  {glyph} {}",
            event.branch,
            " ".repeat(lane - branch_w),
            event.label
        ));
    }
    Some(lines)
}

/// The quoted value of a `key: "value"` attribute inside a gitGraph
/// statement; both `id: "x"` and `id:"x"` spellings work.
fn git_attr(rest: &str, key: &str) -> Option<String> {
    for (index, token) in rest.split_whitespace().enumerate() {
        let (name, value) = match token.strip_suffix(':') {
            Some(name) => (name, rest.split_whitespace().nth(index + 1)),
            None => match token.split_once(':') {
                Some((name, value)) => (name, Some(value)),
                None => continue,
            },
        };
        if name.eq_ignore_ascii_case(key) {
            return value.map(|v| v.trim_matches('"').trim().to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// journey / quadrantChart / xychart-beta — small chart rows
// ---------------------------------------------------------------------------

/// A journey: optional title plus section headers and scored tasks.
pub(super) struct Journey {
    title: Option<String>,
    rows: Vec<JourneyRow>,
}

enum JourneyRow {
    Section(String),
    Task {
        name: String,
        score: u8,
        actors: String,
    },
}

/// Parse a journey diagram: `task: score: actors` rows under `section`
/// headers. Scores must be 1..=5.
pub(super) fn parse_journey(source: &str) -> Option<Journey> {
    let mut title = None;
    let mut rows = Vec::new();
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "journey") {
            continue;
        }
        if starts_with_keyword(line, "title") {
            title = Some(line["title".len()..].trim().to_string());
            continue;
        }
        if starts_with_keyword(line, "section") {
            let section = line["section".len()..].trim().to_string();
            if section.is_empty() {
                return None;
            }
            rows.push(JourneyRow::Section(section));
            continue;
        }
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() < 3 {
            return None;
        }
        let name = parts[0].trim().to_string();
        let score: u8 = parts[1].trim().parse().ok()?;
        if !(1..=5).contains(&score) {
            return None;
        }
        let actors = parts[2..].join(":");
        rows.push(JourneyRow::Task {
            name: truncate_width(&name, 32),
            score,
            actors: truncate_width(actors.trim(), 24),
        });
        if rows.len() > 40 {
            return None;
        }
    }
    if rows.is_empty() {
        return None;
    }
    Some(Journey { title, rows })
}

/// Lay a journey out: section rules, one scored bar row per task.
pub(super) fn layout_journey(j: &Journey, columns: usize) -> Option<Vec<String>> {
    let max_name = j
        .rows
        .iter()
        .map(|row| match row {
            JourneyRow::Section(s) => width::width(s) + 2,
            JourneyRow::Task { name, .. } => width::width(name),
        })
        .max()?
        .max(4);
    let mut lines = Vec::new();
    if let Some(title) = &j.title {
        lines.push(title.clone());
    }
    for row in &j.rows {
        match row {
            JourneyRow::Section(name) => lines.push(format!("── {name}")),
            JourneyRow::Task {
                name,
                score,
                actors,
            } => {
                let pad = " ".repeat(max_name - width::width(name));
                let bar = "█".repeat(*score as usize * 2);
                if max_name + 1 + 10 + 4 + width::width(actors) > columns {
                    return None;
                }
                lines.push(format!("{name}{pad} {bar} {score} {actors}"));
            }
        }
    }
    Some(lines)
}

/// A quadrant chart: four quadrant labels, axes, and points in [0,1]².
pub(super) struct Quadrant {
    title: Option<String>,
    quadrants: [String; 4],
    points: Vec<(String, f64, f64)>,
    x_axis: String,
    y_axis: String,
}

/// Parse a quadrant chart; points are `"name": [x, y]` with floats.
pub(super) fn parse_quadrant(source: &str) -> Option<Quadrant> {
    let mut title = None;
    let mut quadrants: [Option<String>; 4] = [None, None, None, None];
    let mut points = Vec::new();
    let mut x_axis = String::new();
    let mut y_axis = String::new();
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "quadrantchart") {
            continue;
        }
        if starts_with_keyword(line, "title") {
            title = Some(line["title".len()..].trim().to_string());
            continue;
        }
        if starts_with_keyword(line, "x-axis") {
            x_axis = truncate_width(line["x-axis".len()..].trim(), 36);
            continue;
        }
        if starts_with_keyword(line, "y-axis") {
            y_axis = truncate_width(line["y-axis".len()..].trim(), 36);
            continue;
        }
        if let Some(rest) = line.strip_prefix("quadrant-") {
            let (index, label) = rest.split_once(char::is_whitespace)?;
            let index: usize = index.parse().ok()?;
            if !(1..=4).contains(&index) {
                return None;
            }
            quadrants[index - 1] = Some(truncate_width(label.trim(), 14));
            continue;
        }
        // Point: `"name": [x, y]`.
        let (name, coords) = line.split_once(':')?;
        let name = name.trim().trim_matches('"').trim().to_string();
        if name.is_empty() {
            return None;
        }
        let inner = coords.trim().strip_prefix('[')?.strip_suffix(']')?;
        let mut parts = inner.split(',');
        let x: f64 = parts.next()?.trim().parse().ok()?;
        let y: f64 = parts.next()?.trim().parse().ok()?;
        if parts.next().is_some() || !x.is_finite() || !y.is_finite() {
            return None;
        }
        points.push((name, x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)));
        if points.len() > 24 {
            return None;
        }
    }
    if points.is_empty() {
        return None;
    }
    Some(Quadrant {
        title,
        quadrants: quadrants.map(|q| q.unwrap_or_default()),
        points,
        x_axis,
        y_axis,
    })
}

/// Lay a quadrant chart out: a 36×12 grid, the cross at the middle,
/// quadrant labels in the corners, points as `●`, a legend below.
pub(super) fn layout_quadrant(q: &Quadrant, columns: usize) -> Option<Vec<String>> {
    const W: usize = 36;
    const H: usize = 12;
    if columns < W + 2 {
        return None;
    }
    let mid_r = H / 2;
    let mid_c = W / 2;
    let mut rows: Vec<RowCanvas> = (0..H).map(|_| RowCanvas::new(W)).collect();
    for (r, row) in rows.iter_mut().enumerate() {
        for c in 0..W {
            if r == mid_r && c == mid_c {
                row.put(c, "┼");
            } else if r == mid_r {
                row.put(c, "─");
            } else if c == mid_c {
                row.put(c, "│");
            }
        }
    }
    // Quadrant labels: 1 top-right, 2 top-left, 3 bottom-left, 4 bottom-right.
    let place = |row: &mut RowCanvas, text: &str, right: bool| {
        if text.is_empty() {
            return;
        }
        let w = width::width(text);
        let col = if right { W.saturating_sub(w + 1) } else { 1 };
        row.put(col, text.to_string());
    };
    place(&mut rows[1], &q.quadrants[1], false);
    place(&mut rows[1], &q.quadrants[0], true);
    place(&mut rows[H - 1], &q.quadrants[2], false);
    place(&mut rows[H - 1], &q.quadrants[3], true);
    for (_name, x, y) in &q.points {
        let col = (1.0 + x * (W as f64 - 3.0)).round() as usize;
        let row = (1.0 + (1.0 - y) * (H as f64 - 3.0)).round() as usize;
        let col = col.clamp(1, W - 2);
        let row = row.clamp(1, H - 2);
        rows[row].put(col, "●");
    }
    let legend = q
        .points
        .iter()
        .map(|(name, _, _)| format!("● {name}"))
        .collect::<Vec<_>>()
        .join("  ");
    if width::width(&legend) + 2 > columns {
        return None;
    }
    let mut lines: Vec<String> = Vec::new();
    if let Some(title) = &q.title {
        lines.push(title.clone());
    }
    for row in &rows {
        lines.push(row.render());
    }
    if !q.y_axis.is_empty() {
        lines.push(format!("y: {}", q.y_axis));
    }
    if !q.x_axis.is_empty() {
        lines.push(format!("x: {}", q.x_axis));
    }
    lines.push(legend);
    Some(lines)
}

/// An xychart: one numeric series over categorical x values, rendered
/// as scaled bar rows.
pub(super) struct XyChart {
    title: Option<String>,
    categories: Vec<String>,
    values: Vec<f64>,
}

/// Parse an xychart-beta; the first `bar` series wins, then `line`.
pub(super) fn parse_xychart(source: &str) -> Option<XyChart> {
    let mut title = None;
    let mut categories: Option<Vec<String>> = None;
    let mut values: Option<Vec<f64>> = None;
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if starts_with_keyword(line, "xychart-beta") {
            continue;
        }
        if starts_with_keyword(line, "title") {
            title = Some(line["title".len()..].trim().trim_matches('"').to_string());
            continue;
        }
        if starts_with_keyword(line, "x-axis") {
            let rest = line["x-axis".len()..].trim();
            let list = match rest.find('[') {
                Some(open) => &rest[open..],
                None => rest,
            };
            let inner = list.strip_prefix('[')?.strip_suffix(']')?;
            let cats: Vec<String> = inner
                .split(',')
                .map(|c| truncate_width(c.trim().trim_matches('"').trim(), 12))
                .collect();
            if cats.is_empty() || cats.iter().any(|c| c.is_empty()) {
                return None;
            }
            categories = Some(cats);
            continue;
        }
        if starts_with_keyword(line, "y-axis") || starts_with_keyword(line, "tspan") {
            continue;
        }
        if starts_with_keyword(line, "bar") || starts_with_keyword(line, "line") {
            let open = line.find('[')?;
            let inner = line[open..].trim().strip_prefix('[')?.strip_suffix(']')?;
            let series: Vec<f64> = inner
                .split(',')
                .map(|v| v.trim().parse().ok())
                .collect::<Option<_>>()?;
            // A non-finite value (NaN, infinities) poisons the scale:
            // fall back instead of rendering `NaN` rows.
            if series.is_empty() || series.iter().any(|v| !v.is_finite()) {
                return None;
            }
            if values.is_none() {
                values = Some(series);
            }
            continue;
        }
        return None;
    }
    let categories = categories?;
    let values = values?;
    if categories.len() != values.len() || categories.len() > 30 {
        return None;
    }
    Some(XyChart {
        title,
        categories,
        values,
    })
}

/// Lay an xychart out: one scaled bar row per category.
pub(super) fn layout_xychart(chart: &XyChart, columns: usize) -> Option<Vec<String>> {
    let max_label = chart
        .categories
        .iter()
        .map(|c| width::width(c))
        .max()?
        .max(4);
    let bar_max = 20usize;
    let max_value = chart
        .values
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    if !max_value.is_finite() || max_value <= 0.0 {
        return None;
    }
    if max_label + 1 + bar_max + 10 > columns {
        return None;
    }
    let mut lines = Vec::new();
    if let Some(title) = &chart.title {
        lines.push(title.clone());
    }
    for (category, value) in chart.categories.iter().zip(&chart.values) {
        let share = (value / max_value).clamp(0.0, 1.0);
        let bar_len = (share * bar_max as f64).round() as usize;
        let bar = "█".repeat(bar_len);
        let label_col = max_label - width::width(category);
        let value_text = if value.fract() == 0.0 {
            format!("{value:.0}")
        } else {
            format!("{value:.1}")
        };
        lines.push(format!(
            "{}{} {bar} {value_text}",
            category,
            " ".repeat(label_col)
        ));
    }
    Some(lines)
}

/// A parsed gantt chart: title plus `(name, start_day, days)` tasks in
/// declaration order.
pub(super) struct Gantt {
    title: Option<String>,
    tasks: Vec<(String, i64, i64)>,
}

/// Parse a gantt chart. Only `YYYY-MM-DD` dates and day durations
/// (`5d`) are supported; `after x` resolves through earlier tasks.
/// Anything else (unnamed formats, milestones, excludes) falls back.
pub(super) fn parse_gantt(source: &str) -> Option<Gantt> {
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
        // optional and the trailing tag (`done`) is ignored; an
        // id-less task is referenced by its name.
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
pub(super) fn layout_gantt(g: &Gantt, columns: usize) -> Option<Vec<String>> {
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
        // Pad to the display width: char-count padding would misalign
        // wide (CJK) task names.
        let pad = " ".repeat(max_name - width::width(name));
        lines.push(format!("{name}{pad} {bar} {}", &end[5..]));
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
pub(super) struct Pie {
    title: Option<String>,
    slices: Vec<(String, f64)>,
}

/// Parse a pie chart; `None` on malformed slices.
pub(super) fn parse_pie(source: &str) -> Option<Pie> {
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
pub(super) fn layout_pie(pie: &Pie, columns: usize) -> Option<Vec<String>> {
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
