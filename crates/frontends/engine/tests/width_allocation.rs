/*!
 * @file WidthAllocation
 * @description Lock the allocation-free contract of the line measurement.
 *
 * Responsibilities:
 * - Count heap allocations made by the measuring thread.
 * - Assert the line measurement allocates nothing.
 *
 * This module must not depend on: any workspace crate beyond the
 * engine itself, or a network.
 */

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// Allocations made by the current thread. Thread-local on purpose:
    /// the test harness runs tests in parallel, and a process-wide
    /// counter would pick up the other threads' traffic.
    static ALLOCATED_BYTES: Cell<usize> = const { Cell::new(0) };
}

/// Counting allocator: forwards to the system allocator and records the
/// bytes each thread asked for.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// Record one allocation for the calling thread. `try_with` keeps the
/// allocator safe during thread teardown, where the thread-local may
/// already be gone.
fn record(size: usize) {
    let _ = ALLOCATED_BYTES.try_with(|counter| counter.set(counter.get() + size));
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Bytes allocated by the current thread so far.
fn allocated() -> usize {
    ALLOCATED_BYTES.with(Cell::get)
}

#[test]
fn measuring_a_line_allocates_nothing() {
    // A styled line with every token shape the scanner knows: an SGR
    // sequence, plain text, a wide cluster, an OSC hyperlink, and a
    // zero-width marker.
    let line = "\x1b[38;2;1;2;3mhello \x1b[0m世界\x1b]8;;http://example\x07\x1b_wc\x07";
    // Warm the path up so the measurement covers the steady state.
    let expected = tui_engine::width::width(line);
    assert_eq!(expected, 10, "visible width of the sample line");

    let before = allocated();
    let measured = tui_engine::width::width(line);
    let after = allocated();

    assert_eq!(measured, expected, "the measurement must not change");
    assert_eq!(
        after - before,
        0,
        "width() must not allocate: the render path measures every frame row"
    );
}

#[test]
fn wrapping_a_long_line_does_not_pay_for_a_token_buffer() {
    // The scanner used to collect every grapheme into a vector before
    // wrapping, so the cost of a wrap grew with the line length twice
    // over: once for the result lines and once for the token buffer.
    // A long line must now cost about its result lines and no more.
    let short = "\x1b[1mhello world\x1b[0m";
    let long = format!("\x1b[1m{}hello world\x1b[0m", "hello world ".repeat(20));

    let short_lines = tui_engine::width::wrap_line(short, 60);
    let after_short = allocated();
    let long_lines = tui_engine::width::wrap_line(&long, 60);
    let after_long = allocated();

    assert_eq!(short_lines.len(), 1, "{short_lines:?}");
    assert!(long_lines.len() > 1, "{long_lines:?}");
    // The result lines of the long wrap hold ~240 characters and grow
    // their buffers a few times; a token buffer over the same line
    // added more than ten kilobytes on top of that.
    assert!(
        after_long - after_short < 4096,
        "wrapping a 240-column line allocated {} extra bytes",
        after_long - after_short
    );
}
