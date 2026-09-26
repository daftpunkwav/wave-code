# crates/safety/guardrail/ — prompt-injection screening and taint tracking

English | [中文](README.zh.md)

| Item | Role |
|---|---|
| `Cargo.toml` | crate manifest; no dependencies |
| `src/lib.rs` | `scan`/`Signal`/`Severity` over a fixed pattern table, `judge`/`Verdict` (Allow/Warn/Block), `Taint` with `combine` |

Screening is an ASCII case-insensitive substring search over a short,
generic pattern table; `judge` maps the highest matched severity onto
one verdict so every caller decides identically — a single high signal
blocks regardless of how much clean text surrounds it. `Taint` tracks
untrusted tool output into assembled prompts and sticks once present.
The heuristics are speed bumps, not walls: they catch commodity
injections and accidental instruction leakage, and targeted attacks
need layered review on top.
