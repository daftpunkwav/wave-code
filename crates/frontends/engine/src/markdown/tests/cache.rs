//! Render caching and the streaming highlight-cache contract.

use super::*;

/// A highlighter that counts invocations: the streaming contract is
/// that byte-identical blocks highlight once.
struct CountingHighlighter {
    calls: std::rc::Rc<std::cell::Cell<usize>>,
}

impl SyntaxHighlighter for CountingHighlighter {
    fn highlight(&self, code: &str, _lang: Option<&str>) -> Vec<String> {
        self.calls.set(self.calls.get() + 1);
        code.lines().map(|l| l.to_string()).collect()
    }
}

/// A renderer wired to a shared call counter.
fn counting_renderer() -> (Markdown, std::rc::Rc<std::cell::Cell<usize>>) {
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let md = Markdown::new(
        MarkdownStyle::default(),
        Box::new(CountingHighlighter {
            calls: std::rc::Rc::clone(&calls),
        }),
    );
    (md, calls)
}

/// A streamed draft re-renders the whole message on every flush, but
/// only the growing tail block actually changed: earlier blocks must
/// come from the highlight cache instead of re-running the highlighter.
#[test]
fn streaming_flushes_highlight_only_the_new_tail_block() {
    let (mut md, calls) = counting_renderer();
    let prefix = "para\n```rs\nfn a() {}\n```\n";
    md.render(prefix, 40);
    md.render(prefix, 40);
    let lines = md.render(prefix, 40);
    assert!(
        lines.iter().any(|l| strip_ansi(l).contains("fn a() {}")),
        "cached body intact: {lines:?}"
    );
    assert_eq!(calls.get(), 1, "re-renders reuse the cached block");
    // A growing draft appends a second block; the first stays cached.
    let grown = format!("{prefix}```rs\nfn b() {{}}\n```");
    md.render(&grown, 40);
    assert_eq!(calls.get(), 2, "one highlight per distinct block");
    // A different document re-parses block by block; both blocks ride
    // the cache.
    let edited = format!("{prefix}```rs\nfn b() {{}}\n```\ntrailer\n");
    md.render(&edited, 40);
    assert_eq!(calls.get(), 2, "shared blocks hit across documents");
}

/// The cache is bounded and least-recently-used: past the bound the
/// oldest untouched entry goes, a recently touched entry survives.
#[test]
fn highlight_cache_is_bounded_lru() {
    let mut cache = HighlightCache::default();
    let entry = |n: usize| Arc::new(vec![n.to_string()]);
    for n in 0..HIGHLIGHT_CACHE_ENTRIES {
        cache.insert(Some("rs"), &n.to_string(), entry(n));
    }
    // Touch entry 0 so it outlives the next insertion.
    assert!(cache.lookup(Some("rs"), "0").is_some());
    cache.insert(
        Some("rs"),
        &HIGHLIGHT_CACHE_ENTRIES.to_string(),
        entry(HIGHLIGHT_CACHE_ENTRIES),
    );
    assert!(cache.lookup(Some("rs"), "0").is_some(), "recent survives");
    assert!(cache.lookup(Some("rs"), "1").is_none(), "lru evicted");
}

/// A highlighter swap (theme switch) invalidates the cache: cached
/// output is baked in the old palette, so the swapped-in highlighter
/// runs once even for a block the previous one already highlighted.
#[test]
fn highlighter_swap_drops_the_cache() {
    let (mut md, calls) = counting_renderer();
    md.render("```rs\nfn a() {}\n```", 40);
    assert_eq!(calls.get(), 1);
    let swapped = std::rc::Rc::new(std::cell::Cell::new(0));
    md.set_highlighter(Box::new(CountingHighlighter {
        calls: std::rc::Rc::clone(&swapped),
    }));
    md.render("```rs\nfn a() {}\n```", 40);
    assert_eq!(calls.get(), 1, "the old highlighter did not run again");
    assert_eq!(swapped.get(), 1, "the new highlighter ran once");
}

#[test]
fn cache_returns_same_output() {
    let mut md = renderer();
    let first = md.render("cached", 40);
    let second = md.render("cached", 40);
    assert_eq!(first, second);
}

#[test]
fn streamed_full_buffer_rerender_is_stable() {
    let mut md = renderer();
    let full = "- ls - 列出目录内容\n## Shell\n```bash\nls\n```\n";
    // Live drafts arrive prefix by prefix; results are discarded.
    let _ = md.render("- ls - 列出目录内容", 60);
    let _ = md.render("- ls - 列出目录内容\n## Shell", 60);
    let streamed = md.render(full, 60);
    let mut fresh = renderer();
    assert_eq!(streamed, fresh.render(full, 60));
}

#[test]
fn streamed_bold_line_breaks_once_the_buffer_completes() {
    let mut md = renderer();
    // Live draft: only the bullet line has arrived so far.
    let _ = md.render("- lsp_diagnostics - 查看代码诊断信息", 60);
    // The bold heading arrives in a later delta; the accumulated
    // buffer must re-render with the block break.
    let lines = md.render("- lsp_diagnostics - 查看代码诊断信息\n**执行环境**", 60);
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    assert!(
        plain.iter().any(|l| l == "执行环境"),
        "final buffer re-renders with the break: {plain:?}"
    );
}
