# crates/action/browser/ — browser automation behind an async tab seam

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | Crate manifest; no workspace dependencies (thiserror/async-trait/tokio only) |
| `src/lib.rs` | `BrowserSession` trait (open/snapshot/click/fill/close) over driver-neutral `TabState` and `BrowserError`, plus `FakeBrowser`: a scripted fake whose pages render canned text and whose unknown tab ids fail like real drivers |

The crate is data plus a seam: real protocol drivers (CDP, WebDriver)
implement `BrowserSession` without this crate changing. `FakeBrowser`
tracks open tabs so tests observe the same failure modes as a live
driver, and its action log records every call in order.
