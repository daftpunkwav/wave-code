//! The layered band layout (TD) and the column layout (LR) for the
//! shared graph IR, drawn on a per-row canvas.

use super::graph::{BOX_RULE, Diagram, Direction, GraphEdge};
use crate::width;

// ---------------------------------------------------------------------------
// The layered band layout (TD) and column layout (LR)
// ---------------------------------------------------------------------------

/// Dispatch to the direction's layout; `None` when it cannot fit.
pub(super) fn layout(d: &Diagram, columns: usize) -> Option<Vec<String>> {
    match d.direction {
        Direction::TD => layout_td(d, columns),
        Direction::LR => layout_lr(d, columns),
    }
}

/// Longest-path layering by relaxation. A cycle makes the layers grow
/// by one per pass without ever settling: detect it by capping every
/// layer at the node count and fall back to the source view — a cyclic
/// flow (state loops, recursive calls) has no faithful top-down layout
/// in this model.
fn assign_layers(edges: &[GraphEdge], count: usize) -> Option<Vec<usize>> {
    let mut layer = vec![0usize; count];
    for _ in 0..count {
        for edge in edges {
            if layer[edge.to] < layer[edge.from] + 1 {
                layer[edge.to] = layer[edge.from] + 1;
                if layer[edge.to] >= count {
                    return None; // cycle
                }
            }
        }
    }
    Some(layer)
}

/// Group node indices into bands: band `k` holds every node whose
/// longest-path layer is `k`.
fn band_members(layer: &[usize]) -> Vec<Vec<usize>> {
    let max_layer = layer.iter().copied().max().unwrap_or(0);
    let mut bands: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    for (index, depth) in layer.iter().enumerate() {
        bands[*depth].push(index);
    }
    bands
}

/// Box geometry: label lines stacked in the box. The width wraps the
/// widest label line with padding; the height adds the two border rows.
fn box_geometry(labels: &[Vec<String>]) -> (Vec<usize>, Vec<usize>) {
    let box_w: Vec<usize> = labels
        .iter()
        .map(|lines| {
            lines
                .iter()
                .map(|l| width::width(l))
                .max()
                .unwrap_or(2)
                .max(2)
                + 4
        })
        .collect();
    let box_h: Vec<usize> = labels.iter().map(|lines| lines.len() + 2).collect();
    (box_w, box_h)
}

/// Order each band by the mean center of its predecessors' boxes
/// (barycenter; unconnected nodes keep their relative order at the
/// end), then center every band in the canvas — the widest band sets
/// the width. Returns the x origin of each box and the total canvas
/// width.
fn place_band_nodes(
    bands: &mut [Vec<usize>],
    box_w: &[usize],
    edges: &[GraphEdge],
) -> (Vec<usize>, usize) {
    let count = box_w.len();
    let mut x = vec![0usize; count];
    // Widths are placement-independent, so the canvas width is known
    // before any band is placed and every band can center in it.
    let band_width = |band: &[usize]| -> usize {
        let inner: usize = band.iter().map(|&n| box_w[n]).sum();
        inner + 3 * band.len().saturating_sub(1)
    };
    let total_width = bands.iter().map(|b| band_width(b)).max().unwrap_or(0);
    let mut band_of = vec![0usize; count];
    for (depth, band) in bands.iter().enumerate() {
        for &n in band {
            band_of[n] = depth;
        }
    }
    for (depth, band) in bands.iter_mut().enumerate() {
        // Barycenter ordering, top-down: each band sorts by the mean
        // center of its already-placed predecessors, which untangles
        // most crossing clusters before anything is drawn.
        if depth > 0 {
            let mut sum = vec![0u64; count];
            let mut deg = vec![0u32; count];
            for edge in edges {
                if band_of[edge.to] == depth && band_of[edge.from] < depth {
                    sum[edge.to] += (x[edge.from] + box_w[edge.from] / 2) as u64;
                    deg[edge.to] += 1;
                }
            }
            band.sort_by_key(|&n| match deg[n] {
                0 => u64::MAX,
                _ => (sum[n] * 1000) / u64::from(deg[n]),
            });
        }
        let mut cursor = (total_width - band_width(band)) / 2;
        for &index in band.iter() {
            x[index] = cursor;
            cursor += box_w[index] + 3;
        }
    }
    (x, total_width)
}

/// The tallest box in each band sets the band's height (3 when empty).
fn band_heights(bands: &[Vec<usize>], box_h: &[usize]) -> Vec<usize> {
    bands
        .iter()
        .map(|band| band.iter().map(|&n| box_h[n]).max().unwrap_or(3))
        .collect()
}

/// Gap heights: one elbow row per terminating edge, plus a vertical
/// row, the arrowhead row, and — when any edge terminates here — one
/// visible line row above the elbow (a dashed edge must show).
fn gap_heights(edges: &[GraphEdge], layer: &[usize], gaps: usize) -> Vec<usize> {
    let mut gap_height = vec![2usize; gaps];
    let mut terminating = vec![0usize; gaps];
    for edge in edges {
        if layer[edge.to] > layer[edge.from] {
            terminating[layer[edge.to] - 1] += 1;
        }
    }
    for (gap, count) in terminating.iter().enumerate() {
        if *count > 0 {
            gap_height[gap] = gap_height[gap].max(count + 2);
        }
    }
    gap_height
}

/// Lay the diagram out into band rows; `None` when it cannot fit
/// `columns` (the caller falls back to the source view).
fn layout_td(d: &Diagram, columns: usize) -> Option<Vec<String>> {
    let count = d.labels.len();
    if count == 0 || count > 100 {
        return None;
    }
    let layer = assign_layers(&d.edges, count)?;
    let mut bands = band_members(&layer);
    let max_layer = bands.len() - 1;
    // Box geometry: label lines stacked in the box, barycenter-ordered
    // and centered placement inside each band, 3-space gutters between
    // boxes.
    let (box_w, box_h) = box_geometry(&d.labels);
    let (mut x, mut total_width) = place_band_nodes(&mut bands, &box_w, &d.edges);
    if total_width > columns {
        return None;
    }
    let band_h = band_heights(&bands, &box_h);
    let gap_height = gap_heights(&d.edges, &layer, max_layer);

    // One flat canvas per display row: bands contribute an annotation
    // row (subgraph frame titles, only when frames exist) plus their
    // box rows, gaps theirs; `band_top` is the absolute row of each
    // band's annotation row.
    let frame_padded = d.groups.iter().any(|(_, members)| members.len() >= 2);
    let band_lead = usize::from(frame_padded);
    if frame_padded {
        // Side margins so frames clear their member boxes.
        for xi in x.iter_mut() {
            *xi += 2;
        }
        total_width += 5;
        if total_width > columns {
            return None;
        }
    }
    // Straight-edge labels annotate beside the lifeline and often need
    // more room than the widest band provides: grow the canvas (bounded
    // by the terminal budget) instead of truncating them away.
    let mut labeled_width = total_width;
    for edge in &d.edges {
        let (fl, tl) = (layer[edge.from], layer[edge.to]);
        if tl == fl || fl + 1 < tl {
            continue; // only terminating straight edges place labels
        }
        if x[edge.from] + box_w[edge.from] / 2 == x[edge.to] + box_w[edge.to] / 2
            && let Some(text) = &edge.label
        {
            let start = x[edge.to] + box_w[edge.to] / 2 + 2;
            labeled_width = labeled_width.max(start + width::width(text) + 1);
        }
    }
    if labeled_width > columns {
        return None;
    }
    total_width = labeled_width;
    let mut rows: Vec<RowCanvas> = Vec::new();
    let mut band_top = vec![0usize; max_layer + 1];
    for depth in 0..=max_layer {
        band_top[depth] = rows.len();
        for _ in 0..band_lead + band_h[depth] {
            rows.push(RowCanvas::new(total_width));
        }
        if depth < max_layer {
            for _ in 0..gap_height[depth] {
                rows.push(RowCanvas::new(total_width));
            }
        }
    }
    // Absolute row where a band's boxes start / its gaps begin.
    let band_box_top = |depth: usize| band_top[depth] + band_lead;
    let gap_start_of = |gap: usize| band_top[gap] + band_lead + band_h[gap];

    // The lifelines crossing each gap, ascending: a label placed
    // beside a straight vertical stops before the next one, so two
    // lanes never overwrite each other's annotations.
    let mut gap_verticals: Vec<Vec<usize>> = vec![Vec::new(); max_layer];
    for edge in &d.edges {
        let (fl, tl) = (layer[edge.from], layer[edge.to]);
        if tl == fl {
            continue;
        }
        for verts in &mut gap_verticals[fl..tl] {
            verts.push(x[edge.from] + box_w[edge.from] / 2);
            verts.push(x[edge.to] + box_w[edge.to] / 2);
        }
    }
    for verts in &mut gap_verticals {
        verts.sort_unstable();
        verts.dedup();
    }
    let room_after = |verts: &[usize], col: usize| -> usize {
        match verts.iter().find(|&&v| v > col) {
            Some(&next) => next.saturating_sub(col + 1),
            None => total_width.saturating_sub(col + 1),
        }
    };
    let room_before = |verts: &[usize], col: usize| -> usize {
        match verts.iter().rev().find(|&&v| v < col) {
            Some(&prev) => col.saturating_sub(prev + 1),
            None => col,
        }
    };

    // Edges route down the band gaps.
    let mut elbow_slot = vec![0usize; max_layer];
    // Straight (vertically aligned) edges stack their annotations one
    // row above the previous, so two labels never overlap on a row.
    let mut straight_labels = vec![0usize; max_layer];
    for edge in &d.edges {
        let (fl, tl) = (layer[edge.from], layer[edge.to]);
        if tl == fl {
            continue; // same-layer edges do not route
        }
        let from_center = x[edge.from] + box_w[edge.from] / 2;
        let to_center = x[edge.to] + box_w[edge.to] / 2;
        let vfill = edge.style.vertical();
        let head = edge.style.head();
        let hfill = edge.style.horizontal();
        for gap in fl..tl {
            let gap_start = gap_start_of(gap);
            let height = gap_height[gap];
            if gap + 1 < tl {
                // Pass-through: a straight vertical through every gap row.
                for row in &mut rows[gap_start..gap_start + height] {
                    row.put(from_center, vfill);
                }
                continue;
            }
            if from_center == to_center {
                for row in &mut rows[gap_start..gap_start + height - 1] {
                    row.put(from_center, vfill);
                }
                rows[gap_start + height - 1].put(to_center, head);
                // A straight vertical has no horizontal run to carry
                // the label: annotate beside the lifeline instead of
                // dropping it (vertically aligned boxes are common in
                // ER diagrams). Labels stack one row per edge and stop
                // short of the neighboring lifeline — the right side
                // first, the left as the fallback.
                if let Some(text) = &edge.label {
                    let stacked = straight_labels[gap].min(height.saturating_sub(3));
                    straight_labels[gap] += 1;
                    let label_row = gap_start + height - 2 - stacked;
                    let verts = &gap_verticals[gap];
                    let label_w = width::width(text);
                    const MIN_ROOM: usize = 3;
                    let right_start = to_center + 2;
                    let right_room = room_after(verts, right_start);
                    let left_end = to_center.saturating_sub(2);
                    let left_room = room_before(verts, left_end);
                    let (col, room) = if label_w <= right_room {
                        (right_start, label_w)
                    } else if label_w <= left_room {
                        (left_end + 1 - label_w, label_w)
                    } else if right_room >= left_room && right_room >= MIN_ROOM {
                        (right_start, right_room)
                    } else if left_room >= MIN_ROOM {
                        let fit = label_w.min(left_room);
                        (left_end + 1 - fit, fit)
                    } else {
                        (right_start, right_room.min(label_w).max(MIN_ROOM))
                    };
                    rows[label_row].put(col, super::truncate_width(text, room));
                }
                continue;
            }
            let slot = elbow_slot[gap];
            elbow_slot[gap] += 1;
            let elbow = gap_start + slot.min(height.saturating_sub(2));
            for row in &mut rows[gap_start..elbow] {
                row.put(from_center, vfill);
            }
            // Stroke-correct corners: right turn (└,┐), left turn (┘,┌).
            let (corner_from, corner_to) = if from_center < to_center {
                ("└", "┐")
            } else {
                ("┘", "┌")
            };
            rows[elbow].put(from_center, corner_from);
            rows[elbow].put(to_center, corner_to);
            let (left, right) = if from_center < to_center {
                (from_center, to_center)
            } else {
                (to_center, from_center)
            };
            if !draw_horizontal(
                &mut rows[elbow],
                left + 1,
                right,
                edge.label.as_deref(),
                hfill,
            ) && let Some(text) = &edge.label
            {
                // Span too narrow for the label: annotate beside the
                // arrowhead instead of dropping it.
                rows[gap_start + height - 2].put(right + 2, text.clone());
            }
            for row in &mut rows[elbow + 1..gap_start + height - 1] {
                row.put(to_center, vfill);
            }
            rows[gap_start + height - 1].put(to_center, head);
        }
    }

    // Subgraph groups frame their members' bounding box.
    let geo = &GroupFrameGeometry {
        layer: &layer,
        x: &x,
        box_w: &box_w,
        band_h: &band_h,
        band_top: &band_top,
        band_lead,
        total_width,
    };
    for (title, members) in &d.groups {
        let _ = draw_group_frame(&mut rows, members, title, geo);
    }

    // Boxes paint last so corners survive edge and frame lines.
    for (depth, band) in bands.iter().enumerate() {
        let top = band_box_top(depth);
        for &index in band {
            let (bx, bw) = (x[index], box_w[index]);
            rows[top].put(bx, format!("┌{}┐", "─".repeat(bw - 2)));
        }
        let body_rows = band_h[depth] - 2;
        for (row_index, row) in rows[top + 1..top + 1 + body_rows].iter_mut().enumerate() {
            for &index in band {
                if let Some(line) = d.labels[index].get(row_index) {
                    if line == BOX_RULE {
                        // The title/members separator spans the box.
                        let inner = box_w[index].saturating_sub(2);
                        row.put(x[index], format!("│{}│", "─".repeat(inner)));
                    } else {
                        let pad = box_w[index] - 2 - width::width(line);
                        let left = pad / 2;
                        row.put(
                            x[index],
                            format!("│{}{line}{}│", " ".repeat(left), " ".repeat(pad - left)),
                        );
                    }
                }
            }
        }
        for &index in band {
            let (bx, bw) = (x[index], box_w[index]);
            rows[top + 1 + box_h[index] - 2].put(bx, format!("└{}┘", "─".repeat(bw - 2)));
        }
    }

    Some(rows.iter().map(RowCanvas::render).collect())
}

/// The layout context a group frame reads: each node's band layer, x
/// origin, and box width, plus the band stack geometry (band heights,
/// annotation-row offsets, and the total canvas width).
struct GroupFrameGeometry<'a> {
    layer: &'a [usize],
    x: &'a [usize],
    box_w: &'a [usize],
    band_h: &'a [usize],
    band_top: &'a [usize],
    band_lead: usize,
    total_width: usize,
}

/// Draw a subgraph group's bounding frame. The top border rides the
/// band's annotation row (exclusively the frame's), so the title never
/// collides with member boxes; sides and bottom are per-cell glyphs
/// that member boxes (painted later) cut their own corners out of.
fn draw_group_frame(
    rows: &mut [RowCanvas],
    members: &[usize],
    title: &str,
    geo: &GroupFrameGeometry<'_>,
) -> Option<()> {
    if members.len() < 2 {
        return Some(());
    }
    let min_x = members.iter().map(|&n| geo.x[n]).min()?;
    let max_end = members.iter().map(|&n| geo.x[n] + geo.box_w[n]).max()?;
    let top_band = members.iter().map(|&n| geo.layer[n]).min()?;
    let bottom_band = members.iter().map(|&n| geo.layer[n]).max()?;
    let left = min_x.saturating_sub(2);
    let right = max_end + 1;
    if right + 1 >= geo.total_width || right <= left + 4 {
        return Some(());
    }
    let top_row = geo.band_top[top_band];
    let bottom_row = geo.band_top[bottom_band] + geo.band_lead + geo.band_h[bottom_band] - 1;
    if bottom_row >= rows.len() {
        return Some(());
    }
    // Top border with the inline title on the annotation row.
    let mut top = format!("┌─ {title} ");
    if width::width(&top) > right - left {
        top = String::from("┌");
    }
    while width::width(&top) < right - left {
        top.push('─');
    }
    top.push('┐');
    rows[top_row].put(left, top);
    // Sides and bottom.
    for row in &mut rows[top_row + 1..bottom_row] {
        row.put(left, "│");
        row.put(right, "│");
    }
    rows[bottom_row].put(left, "└");
    for col in left + 1..right {
        rows[bottom_row].put(col, "─");
    }
    rows[bottom_row].put(right, "┘");
    Some(())
}

/// Fill a horizontal run with dashes, swapping the middle for an edge
/// label when one fits. Runs either direction; an overlapping vertical
/// at the run's own column is kept (the corner glyph wins).
fn draw_horizontal(
    row: &mut RowCanvas,
    start: usize,
    end: usize,
    label: Option<&str>,
    fill: &str,
) -> bool {
    if end <= start {
        return false;
    }
    for col in start..end {
        row.put(col, fill);
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

/// Lay the diagram out left-to-right: layers become columns, boxes
/// stack inside a column, edges run through the column gutters with
/// one elbow each. Boxes paint last, so an edge crossing a taller
/// neighbor's column is clipped rather than corrupting the boxes.
fn layout_lr(d: &Diagram, columns: usize) -> Option<Vec<String>> {
    let count = d.labels.len();
    if count == 0 || count > 100 {
        return None;
    }
    let layer = assign_layers(&d.edges, count)?;
    let mut cols = band_members(&layer);
    let max_layer = layer.iter().copied().max().unwrap_or(0);

    let (box_w, box_h) = box_geometry(&d.labels);
    // Gutters carry elbow routing and edge labels: a fixed 3-cell gutter
    // drops every label (draw_horizontal has no room to place it). Widen a
    // gutter to fit the widest labeled edge crossing it, capped so a huge
    // label cannot push the frame past the terminal.
    let mut gutters = vec![3usize; max_layer.max(1)];
    for edge in &d.edges {
        if let Some(label) = &edge.label {
            let fl = layer[edge.from].min(layer[edge.to]);
            if fl < max_layer {
                // The labeled horizontal segment between the elbow and the
                // arrowhead loses a cell on each side (the elbow join and
                // the head), so the gutter needs the label width plus four.
                let w = width::width(label).min(24) + 4;
                gutters[fl] = gutters[fl].max(w);
            }
        }
    }
    let mut col_x = vec![0usize; max_layer + 1];
    let mut col_w = vec![0usize; max_layer + 1];
    let mut total_width = 0usize;
    for depth in 0..=max_layer {
        col_w[depth] = cols[depth].iter().map(|&n| box_w[n]).max().unwrap_or(2);
        col_x[depth] = total_width;
        total_width += col_w[depth];
        // Inter-column gutters only: the final column carries no trailing
        // gutter, so the canvas width includes it (and the arrowhead drawn
        // against labeled edges) exactly.
        if depth < max_layer {
            total_width += gutters[depth];
        }
    }
    let mut y = vec![0usize; count];
    let column_height = |column: &[usize]| -> usize {
        let inner: usize = column.iter().map(|&n| box_h[n]).sum();
        inner + 2 * column.len().saturating_sub(1)
    };
    let total_height = {
        let tallest = cols.iter().map(|c| column_height(c)).max().unwrap_or(0);
        // Barycenter ordering, left-to-right: each column sorts by the
        // mean row-center of its already-placed predecessors, then
        // centers vertically in the canvas — the tallest column sets
        // the height.
        let mut column_of = vec![0usize; count];
        for (depth, column) in cols.iter().enumerate() {
            for &n in column {
                column_of[n] = depth;
            }
        }
        for (depth, column) in cols.iter_mut().enumerate() {
            if depth > 0 {
                let mut sum = vec![0u64; count];
                let mut deg = vec![0u32; count];
                for edge in &d.edges {
                    if column_of[edge.to] == depth && column_of[edge.from] < depth {
                        sum[edge.to] += (y[edge.from] + box_h[edge.from] / 2) as u64;
                        deg[edge.to] += 1;
                    }
                }
                column.sort_by_key(|&n| match deg[n] {
                    0 => u64::MAX,
                    _ => (sum[n] * 1000) / u64::from(deg[n]),
                });
            }
            let mut cursor = (tallest - column_height(column)) / 2;
            for &index in column.iter() {
                y[index] = cursor;
                cursor += box_h[index] + 2;
            }
        }
        tallest
    };
    if total_width > columns || total_height > 120 || total_height == 0 {
        return None;
    }
    let mut rows: Vec<RowCanvas> = (0..total_height)
        .map(|_| RowCanvas::new(total_width))
        .collect();

    // The vertical routing lane through each gutter: between the two
    // columns, clear of every box.
    let lane = |gap: usize| col_x[gap] + col_w[gap] + gutters[gap] / 2;
    for edge in &d.edges {
        let (fl, tl) = (layer[edge.from], layer[edge.to]);
        if tl == fl {
            continue;
        }
        let from_row = y[edge.from] + box_h[edge.from] / 2;
        let to_row = y[edge.to] + box_h[edge.to] / 2;
        let src_right = col_x[fl] + box_w[edge.from];
        let dst_left = col_x[tl];
        let vfill = edge.style.vertical();
        let hfill = edge.style.horizontal();
        let label = edge.label.as_deref();
        if from_row == to_row {
            // Straight run into the target's left edge.
            let head_col = dst_left.saturating_sub(1);
            let placed = draw_horizontal(&mut rows[from_row], src_right, head_col, label, hfill);
            if !placed && let Some(text) = label {
                // Label wider than the run: truncate into it instead
                // of dropping it.
                let room = head_col.saturating_sub(src_right + 2);
                if room >= 3 {
                    rows[from_row].put(src_right + 1, super::truncate_width(text, room));
                }
            }
            if head_col < total_width {
                rows[from_row].put(head_col, "▶");
            }
            continue;
        }
        if tl > fl + 1 {
            // A skip-level edge routes through the inter-column
            // gutters: a straight run would pierce every intermediate
            // box, and boxes paint last, so the line would read as
            // connecting the wrong box. Cross on a row no intermediate
            // box occupies, nearest to the source exit; wall-to-wall
            // columns fall back to the straight run.
            let crossing = (0..total_height)
                .filter(|&r| r != from_row && r != to_row)
                .min_by_key(|&r| (r as isize - from_row as isize).unsigned_abs())
                .filter(|&r| {
                    (fl + 1..tl).all(|k| cols[k].iter().all(|&n| r < y[n] || r >= y[n] + box_h[n]))
                });
            let head_col = dst_left.saturating_sub(1);
            let (start_lane, end_lane) = (lane(fl), lane(tl - 1));
            if let Some(r) = crossing {
                for col in src_right..start_lane {
                    rows[from_row].put(col, hfill);
                }
                rows[from_row].put(start_lane, if r > from_row { "┐" } else { "┘" });
                let (top, bottom) = (r.min(from_row), r.max(from_row));
                for row in &mut rows[top + 1..bottom] {
                    row.put(start_lane, vfill);
                }
                rows[r].put(start_lane, if r > from_row { "└" } else { "┌" });
                let placed = draw_horizontal(&mut rows[r], start_lane + 1, end_lane, label, hfill);
                if !placed && let Some(text) = label {
                    let room = end_lane.saturating_sub(start_lane + 3);
                    if room >= 3 {
                        rows[r].put(start_lane + 1, super::truncate_width(text, room));
                    }
                }
                rows[r].put(end_lane, if to_row > r { "┐" } else { "┘" });
                let (top, bottom) = (to_row.min(r), to_row.max(r));
                for row in &mut rows[top + 1..bottom] {
                    row.put(end_lane, vfill);
                }
                rows[to_row].put(end_lane, if to_row > r { "└" } else { "┌" });
                for col in end_lane + 1..head_col {
                    rows[to_row].put(col, hfill);
                }
            } else {
                draw_horizontal(&mut rows[from_row], src_right, head_col, label, hfill);
                if head_col < total_width {
                    rows[from_row].put(head_col, "▶");
                }
            }
            continue;
        }
        // Adjacent columns, different rows: elbow in the gutter.
        let gx = col_x[fl] + col_w[fl] + 1;
        for col in src_right..gx {
            rows[from_row].put(col, hfill);
        }
        rows[from_row].put(gx, if to_row > from_row { "┐" } else { "┘" });
        let (top, bottom) = (from_row.min(to_row), from_row.max(to_row));
        for row in &mut rows[top + 1..bottom] {
            row.put(gx, vfill);
        }
        rows[to_row].put(gx, if to_row > from_row { "└" } else { "┌" });
        let head_col = dst_left.saturating_sub(1);
        draw_horizontal(&mut rows[to_row], gx + 1, head_col, label, hfill);
        if head_col < total_width {
            rows[to_row].put(head_col, "▶");
        }
    }

    // Boxes paint last.
    for depth in 0..=max_layer {
        for &index in &cols[depth] {
            let (bx, bw, by, bh) = (col_x[depth], box_w[index], y[index], box_h[index]);
            rows[by].put(bx, format!("┌{}┐", "─".repeat(bw - 2)));
            for (row_index, row) in rows[by + 1..by + bh - 1].iter_mut().enumerate() {
                if let Some(line) = d.labels[index].get(row_index) {
                    if line == BOX_RULE {
                        // The title/members separator spans the box.
                        let inner = bw.saturating_sub(2);
                        row.put(bx, format!("│{}│", "─".repeat(inner)));
                    } else {
                        let pad = bw - 2 - width::width(line);
                        let left = pad / 2;
                        row.put(
                            bx,
                            format!("│{}{line}{}│", " ".repeat(left), " ".repeat(pad - left)),
                        );
                    }
                }
            }
            rows[by + bh - 1].put(bx, format!("└{}┘", "─".repeat(bw - 2)));
        }
    }
    Some(rows.iter().map(RowCanvas::render).collect())
}

/// One display row: cells placed at exact display columns, last write
/// wins. Renders with single-width spaces in the gaps.
pub(super) struct RowCanvas {
    placements: Vec<(usize, String)>,
    width: usize,
}

impl RowCanvas {
    pub(super) fn new(width: usize) -> Self {
        Self {
            placements: Vec::new(),
            width,
        }
    }

    /// Place `text` at display column `col`, dropping overlapped cells.
    pub(super) fn put(&mut self, col: usize, text: impl Into<String>) {
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
    pub(super) fn render(&self) -> String {
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
