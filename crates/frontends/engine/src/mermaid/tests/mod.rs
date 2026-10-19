//! Tests for the mermaid renderer ([`crate::mermaid`]), grouped by
//! concern: the keyword dispatch and fit fallbacks, the shared graph
//! engine (flowchart/state/class/ER/C4/requirement), the sequence
//! lanes, and the chart-row kinds.

/// One flowchart exercised by the band-layout assertions.
const FLOW: &str = "\
graph TD
    A[开始] --> B{是否?}
    B -- 是 --> C[执行]
    B -- 否 --> D[跳过]
    C --> E[结束]
    D --> E";

mod charts;
mod dispatch;
mod flowchart;
mod sequence;
mod shared_engine;
