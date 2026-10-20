/*!
 * @file RenderAllocation
 * @description Lock the per-frame allocation budget of the transcript
 * components.
 *
 * Responsibilities:
 * - Count heap allocations made by the measuring thread.
 * - Assert the per-frame render path does not copy whole message
 *   bodies.
 *
 * This module must not depend on: a session, a terminal, or the network.
 */

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use console_ui::messages::{ExpandedFlag, Thinking};
use console_ui::theme;
use tui_engine::component::Component;

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
fn the_live_thinking_block_does_not_copy_the_reasoning_buffer() {
    theme::set(theme::Theme::dark());
    let body = "reasoning line\n".repeat(2000);
    let mut block = Thinking::live(ExpandedFlag::new());
    block.push(&body);

    // The live block shows a two-line tail, so the frame itself is
    // small: the whole reasoning buffer must not be copied into the
    // render cache.
    let before = allocated();
    block.render(60);
    let after = allocated();

    assert!(
        after - before < body.len(),
        "rendering the live block copied the reasoning buffer ({} bytes for a {} byte body)",
        after - before,
        body.len()
    );
}

#[test]
fn mirroring_an_unchanged_reasoning_buffer_copies_nothing() {
    theme::set(theme::Theme::dark());
    let body = "reasoning line\n".repeat(2000);
    let mut block = Thinking::live(ExpandedFlag::new());
    block.set_text(&body);

    // The render loop mirrors the same buffer on every frame; the
    // component must compare it, not copy it.
    let before = allocated();
    block.set_text(&body);
    let after = allocated();

    assert_eq!(
        after - before,
        0,
        "mirroring an unchanged buffer must not copy it"
    );
}
