/*!
 * @file RequestEstimateAllocation
 * @description Lock the per-request cost of the eviction gateway's usage
 * estimate.
 *
 * Responsibilities:
 * - Count heap allocations made by the measuring thread.
 * - Assert the request estimate reads the request instead of copying it.
 *
 * This module must not depend on: a session, a network, or model
 * credentials.
 */

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use operations_bootstrap::evicting_gateway::EvictingGateway;
use runtime_runner::{ModelGateway, SampleError, SampleRequest, SampleResponse};
use state_store::{Block, HistoryEntry, Role};
use wavecode_context::EvictionConfig;

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

/// Gateway that accepts every request and answers with nothing.
struct AcceptingGateway;

#[async_trait::async_trait]
impl ModelGateway for AcceptingGateway {
    async fn sample(&self, _request: SampleRequest) -> Result<SampleResponse, SampleError> {
        Ok(SampleResponse::default())
    }
}

/// A gateway that never evicts: the request reaches the inner gateway
/// unchanged, so the measurement window covers the usage estimate only.
fn gateway() -> EvictingGateway<AcceptingGateway> {
    EvictingGateway::with_config(
        AcceptingGateway,
        EvictionConfig {
            // nosemgrep: rust.lang.security.hard-coded-tokens
            soft_threshold_tokens: u64::MAX,
            ..EvictionConfig::default()
        },
    )
}

#[tokio::test]
async fn the_usage_estimate_reads_the_request_instead_of_copying_it() {
    // One megabyte of history: the request the model sees is this big on
    // every sample, and the estimate runs on every sample.
    let body = "a".repeat(1_000_000);
    let request = SampleRequest {
        system: "system prompt".to_string(),
        messages: vec![HistoryEntry {
            role: Role::User,
            blocks: vec![Block::Text(body)],
        }],
        tools: Vec::new(),
        output_cap: 0,
        notes: Vec::new(),
    };

    let before = allocated();
    gateway()
        .sample(request)
        .await
        .expect("the sample is accepted");
    let after = allocated();

    assert!(
        after - before < 64 * 1024,
        "the usage estimate copied the request ({} bytes for a 1 MB history)",
        after - before
    );
}
