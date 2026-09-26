# crates/infrastructure/ratelimit/ — token-bucket rate limiting over explicit time

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `src/lib.rs` | `TokenBucket` (`try_new` validated construction, `try_acquire`/`try_acquire_n`, `time_until_available` retry hint, `available`) and `BucketConfigError` |

Rate limiting here is pure arithmetic over caller-supplied timestamps:
the bucket never reads a clock, so behavior is deterministic under test
and a caller can drive it from any time source. Refill is lazy and
clamped at capacity; a backwards clock never overfills the bucket.
`try_new` rejects a zero capacity, a non-finite or negative refill
rate, and a non-finite timestamp instead of clamping them silently.
`time_until_available` is a pure query — it never mutates state — and
returns `None` when a request can never be satisfied (larger than the
burst capacity, or a bucket that never refills). The crate depends only
on `thiserror`; its in-workspace consumer today is
`operations/bootstrap`.
