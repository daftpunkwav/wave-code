//! The chart-row kinds: gitGraph, mindmap, timeline, journey,
//! quadrantChart, xychart-beta, and gantt.

use super::super::*;

#[test]
fn git_graphs_render_branch_timelines() {
    let src = "gitGraph\n    commit id: \"one\"\n    branch develop\n    commit id: \"two\"\n    checkout main\n    merge develop";
    let lines = render_diagram(src, 80).expect("gitGraph renders");
    let joined = lines.join("\n");
    for label in ["main", "develop", "one", "two", "merge develop"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
    assert!(joined.contains('●'), "merge glyph: {joined}");
    assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
}

/// Unnamed commits fall back to sequence numbers instead of
/// dropping the event.
#[test]
fn git_graphs_name_unnamed_commits() {
    let src = "gitGraph\n    commit\n    commit tag: \"v1\"";
    let lines = render_diagram(src, 80).expect("unnamed commits render");
    let joined = lines.join("\n");
    assert!(joined.contains("#1") && joined.contains("v1"), "{joined}");
}

#[test]
fn mindmaps_render_as_trees() {
    let src = "mindmap\n  root((编程语言))\n    静态类型\n      Java\n      Go\n    动态类型\n      Python";
    let lines = render_diagram(src, 80).expect("mindmap renders");
    let joined = lines.join("\n");
    for label in ["编程语言", "静态类型", "Java", "Go", "动态类型", "Python"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
    assert!(
        joined.contains("├─") || joined.contains("└─"),
        "connectors: {joined}"
    );
    assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
}

#[test]
fn timelines_render_periods_and_events() {
    let src = "timeline\n    title 历史\n    2021 : 发现漏洞\n         : 修复\n    2022 : 发布";
    let lines = render_diagram(src, 80).expect("timeline renders");
    let joined = lines.join("\n");
    for label in ["历史", "2021", "发现漏洞", "修复", "2022", "发布"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
    assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
}

#[test]
fn journeys_render_scored_task_rows() {
    let src =
        "journey\n    title 工作日\n    section 早晨\n        泡茶: 5: 我\n        上楼: 3: 我";
    let lines = render_diagram(src, 80).expect("journey renders");
    let joined = lines.join("\n");
    for label in ["工作日", "早晨", "泡茶", "上楼", "我"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
    assert!(joined.contains('█'), "score bars: {joined}");
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
fn quadrant_charts_render_grid_and_points() {
    let src = "quadrantChart\n    title 影响力\n    x-axis Low --> High\n    y-axis Low --> High\n    quadrant-1 Plan\n    quadrant-2 Promote\n    quadrant-3 Demo\n    quadrant-4 Build\n    \"Point A\": [0.3, 0.7]\n    \"Point B\": [0.8, 0.2]";
    let lines = render_diagram(src, 80).expect("quadrant renders");
    let joined = lines.join("\n");
    for label in ["Plan", "Promote", "Demo", "Build", "Point A", "Point B"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
    assert!(joined.contains('┼'), "axis cross: {joined}");
    assert!(lines.iter().all(|l| width::width(l) <= 80), "{lines:?}");
}

/// Points sharing one cell spread onto neighboring cells instead of
/// overwriting each other: three coincident points draw three glyphs.
#[test]
fn overlapping_quadrant_points_stay_visible() {
    let src = "quadrantChart\n    x-axis a --> b\n    y-axis c --> d\n    \"P1\": [0.5, 0.5]\n    \"P2\": [0.5, 0.5]\n    \"P3\": [0.5, 0.5]";
    let lines = render_diagram(src, 80).expect("coincident points render");
    let dots = lines
        .iter()
        .take_while(|l| !l.starts_with("y:") && !l.starts_with("x:"))
        .map(|l| l.matches('●').count())
        .sum::<usize>();
    assert_eq!(dots, 3, "three visible glyphs: {lines:?}");
}

/// A legend wider than the terminal wraps instead of falling back to
/// the source view, and every wrapped row fits the budget.
#[test]
fn quadrant_legends_wrap_instead_of_falling_back() {
    let mut src = String::from("quadrantChart\n    x-axis a --> b\n    y-axis c --> d\n");
    for n in 0..6 {
        src.push_str(&format!(
            "    \"point-with-a-long-name-{n}\": [0.{n}, 0.9]\n"
        ));
    }
    let lines = render_diagram(&src, 60).expect("wrapped legend renders");
    let legend: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("point-with-a-long-name"))
        .collect();
    assert!(legend.len() >= 2, "legend split across rows: {lines:?}");
    assert!(
        lines.iter().all(|l| width::width(l) <= 60),
        "every row fits: {lines:?}"
    );
}

#[test]
fn xycharts_render_scaled_bar_rows() {
    let src = "xychart-beta\n    title \"销量\"\n    x-axis [1月, 2月, 3月]\n    y-axis \"销量\" 0 --> 50\n    bar [10, 30, 20]";
    let lines = render_diagram(src, 80).expect("xychart renders");
    let joined = lines.join("\n");
    assert!(joined.contains("销量"), "{joined}");
    for label in ["1月", "2月", "3月"] {
        assert!(joined.contains(label), "{label}: {joined}");
    }
    assert!(joined.contains('█'), "bars: {joined}");
    let bar_of = |name: &str| {
        lines
            .iter()
            .find(|l| l.contains(name))
            .map(|l| l.matches('█').count())
            .unwrap()
    };
    assert!(bar_of("2月") > bar_of("1月"), "scaled to max: {lines:?}");
}

/// A NaN series value poisons the scale: fall back instead of
/// rendering a `NaN` row.
#[test]
fn xychart_nan_values_fall_back_to_none() {
    let src = "xychart-beta\n    x-axis [a, b]\n    bar [NaN, 10]";
    assert!(render_diagram(src, 80).is_none(), "NaN series falls back");
}

/// A leading status tag (`done`) precedes the id, and `after x` starts
/// the dependent task when `x` ends, not when it starts.
#[test]
fn gantt_tags_and_after_dependencies_resolve() {
    let src = "gantt\n    title plan\n    Task1 :done, t1, 2024-01-01, 3d\n    Task2 :crit, done, after t1, 2d";
    let lines = render_diagram(src, 80).expect("gantt with tag and after renders");
    let joined = lines.join("\n");
    assert!(joined.contains("Task1"), "{joined}");
    assert!(joined.contains("Task2"), "{joined}");
    // Task1 ends 2024-01-04; `after t1` starts Task2 there, so its bar
    // ends 2024-01-06 (not 01-03, which a start-aligned read would give).
    assert!(joined.contains("01-04"), "{joined}");
    assert!(joined.contains("01-06"), "{joined}");
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

/// Bars sit at their offset from the earliest start: two same-length
/// tasks with staggered dates draw their bars at staggered positions.
#[test]
fn gantt_bars_offset_by_their_start_dates() {
    let src = "gantt\n    t1 :a, 2026-01-01, 2d\n    t2 :b, 2026-01-10, 2d";
    let lines = render_diagram(src, 80).expect("staggered gantt renders");
    let bar_col = |name: &str| {
        lines
            .iter()
            .find(|l| l.contains(name))
            .and_then(|l| l.find('\u{2588}'))
            .expect("a bar row")
    };
    assert!(
        bar_col("t2") > bar_col("t1"),
        "later start sits further right: {lines:?}"
    );
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
