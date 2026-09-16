//! Application state shared by chrome components: session identity,
//! streaming phase, context usage, and queued input.

use std::path::PathBuf;

/// What the harness is doing right now; drives the activity indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamingPhase {
    /// Nothing running.
    Idle,
    /// Turn accepted, waiting for first model output.
    Waiting,
    /// Extended thinking is streaming.
    Thinking,
    /// Assistant text is streaming.
    Composing,
    /// A tool call is running.
    Tool,
}

/// Mutable UI state snapshot; components read it during rendering.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Model display name.
    pub model_name: String,
    /// Session working directory.
    pub cwd: PathBuf,
    /// Permission mode wire name (`plan` / `guarded` / `auto`).
    pub permission_mode: String,
    /// Connected MCP server names.
    pub mcp_servers: Vec<String>,
    /// Current streaming phase.
    pub phase: StreamingPhase,
    /// Tokens estimated inside the context window (last settle).
    pub context_used: Option<u64>,
    /// Session context window size.
    pub context_window: Option<u64>,
    /// User messages queued while a turn runs.
    pub queued: Vec<String>,
    /// Tool output expansion flag (Ctrl+O toggles).
    pub expanded: bool,
}

impl AppState {
    /// Initial state from session context.
    pub fn new(
        model_name: String,
        cwd: PathBuf,
        permission_mode: String,
        mcp_servers: Vec<String>,
    ) -> Self {
        Self {
            model_name,
            cwd,
            permission_mode,
            mcp_servers,
            phase: StreamingPhase::Idle,
            context_used: None,
            context_window: None,
            queued: Vec::new(),
            expanded: false,
        }
    }

    /// True when a turn is in flight.
    pub fn busy(&self) -> bool {
        self.phase != StreamingPhase::Idle
    }
}

/// Format a token count 1024-based: `262144` → `256k`, `1.5M` above a
/// mebibyte. Counts from 100k upward render as whole k.
pub fn format_tokens(tokens: u64) -> String {
    const K: f64 = 1024.0;
    const M: f64 = 1024.0 * 1024.0;
    let value = tokens as f64;
    if value >= M {
        let m = value / M;
        if m >= 10.0 {
            format!("{m:.0}M")
        } else {
            format!("{m:.1}M")
        }
    } else if value >= 100.0 * K {
        format!("{}k", (value / K).round() as u64)
    } else if value >= K {
        format!("{:.1}k", value / K)
    } else {
        tokens.to_string()
    }
}

/// Context percentage, ceil, clamped to 100.
pub fn context_percent(used: u64, window: u64) -> u64 {
    if window == 0 {
        return 0;
    }
    ((used as f64 / window as f64 * 100.0).ceil() as u64).min(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_formatting_uses_1024_base() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1024), "1.0k");
        assert_eq!(format_tokens(20_000), "19.5k");
        assert_eq!(format_tokens(262_144), "256k");
        assert_eq!(format_tokens(200_000), "195k");
        assert_eq!(format_tokens(1_500_000), "1.4M");
        assert_eq!(format_tokens(15_000_000), "14M");
    }

    #[test]
    fn percent_ceils_and_clamps() {
        assert_eq!(context_percent(84_000, 200_000), 42);
        assert_eq!(context_percent(1, 200_000), 1);
        assert_eq!(context_percent(200_000, 200_000), 100);
        assert_eq!(context_percent(500_000, 200_000), 100);
        assert_eq!(context_percent(10, 0), 0);
    }

    #[test]
    fn busy_tracks_phase() {
        let mut state = AppState::new(
            "m".to_string(),
            PathBuf::from("."),
            "guarded".to_string(),
            Vec::new(),
        );
        assert!(!state.busy());
        state.phase = StreamingPhase::Composing;
        assert!(state.busy());
    }
}
