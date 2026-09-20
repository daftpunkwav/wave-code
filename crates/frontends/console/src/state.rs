//! Application state shared by chrome components: session identity,
//! streaming phase, context usage, and queued input.

use std::path::PathBuf;

/// What the harness is doing right now; drives the input pulse.
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

/// Completion state of one todo entry (mirrors the `todowrite` tool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Currently being worked on.
    InProgress,
    /// Finished.
    Completed,
}

/// One todo entry, as rewritten wholesale by `todowrite` calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoEntry {
    /// What to do (single line).
    pub content: String,
    /// Current completion state.
    pub status: TodoStatus,
}

/// Cumulative session token usage, accumulated from `TokenCount`
/// samples (each sample reports its own input/output split).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Sum of sample input tokens, prompt-cache traffic included (the
    /// provider adapters fold cache counters into the input total).
    pub input: u64,
    /// Sum of sample output tokens.
    pub output: u64,
    /// Sum of prompt-cache read tokens (a breakdown of `input`).
    pub cache_read: u64,
    /// Sum of prompt-cache write tokens (a breakdown of `input`).
    pub cache_creation: u64,
}

impl TokenUsage {
    /// Input + output. Cache read/write are a breakdown of the input column,
    /// so they are shown beside it rather than added to the total.
    pub fn total(self) -> u64 {
        self.input + self.output
    }
}

/// One exported dialogue entry: who said what, in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogueEntry {
    /// True for a user message, false for an assistant message.
    pub from_user: bool,
    /// Full message text.
    pub text: String,
}

/// Mutable UI state snapshot; components read it during rendering.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Model display name.
    pub model_name: String,
    /// Provider id the current model samples through.
    pub provider_id: String,
    /// Current reasoning-effort level (provider-specific; `None` when
    /// unset or unsupported).
    pub thinking_effort: Option<String>,
    /// Session id (uuid; rotates on `/new`).
    pub session_id: String,
    /// Session display title (`/title`), when set.
    pub session_title: Option<String>,
    /// Home directory backing the session journal (`None` disables
    /// journaling, resume, fork, and title persistence).
    pub home: Option<PathBuf>,
    /// Session working directory.
    pub cwd: PathBuf,
    /// Permission mode wire name (`plan` / `auto` / `wave`).
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
    /// Session todo list (`todowrite` rewrites it wholesale).
    pub todos: Vec<TodoEntry>,
    /// Todo panel expansion (Ctrl+T toggles when the list overflows).
    pub todo_expanded: bool,
    /// Cumulative token usage across the session.
    pub usage: TokenUsage,
    /// Git branch (or short detached sha) of the workspace, refreshed
    /// on turn starts; `None` outside a repository.
    pub git_branch: Option<String>,
    /// One-line newer-release notice (footer tip slot); filled from the
    /// update-check slot on a later tick, `None` until it arrives.
    pub update_notice: Option<String>,
    /// Full user/assistant dialogue for `/export` and `/copy`; unlike
    /// the transcript this is never trimmed.
    pub dialogue: Vec<DialogueEntry>,
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
            provider_id: String::new(),
            thinking_effort: None,
            session_id: String::new(),
            session_title: None,
            home: None,
            cwd,
            permission_mode,
            mcp_servers,
            phase: StreamingPhase::Idle,
            context_used: None,
            context_window: None,
            queued: Vec::new(),
            expanded: false,
            todos: Vec::new(),
            todo_expanded: false,
            usage: TokenUsage::default(),
            git_branch: None,
            update_notice: None,
            dialogue: Vec::new(),
        }
    }

    /// True when a turn is in flight.
    pub fn busy(&self) -> bool {
        self.phase != StreamingPhase::Idle
    }

    /// Record one dialogue message for export.
    pub fn push_dialogue(&mut self, from_user: bool, text: &str) {
        self.dialogue.push(DialogueEntry {
            from_user,
            text: text.to_string(),
        });
    }

    /// Drop the last `turns` user messages and everything after each
    /// from the exportable dialogue. Returns the number of user
    /// messages actually removed (fewer when the dialogue runs out).
    pub fn rewind_dialogue(&mut self, turns: u32) -> usize {
        let user_positions: Vec<usize> = self
            .dialogue
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.from_user)
            .map(|(index, _)| index)
            .collect();
        let drop = (turns as usize).min(user_positions.len());
        if drop == 0 {
            return 0;
        }
        let cutoff = user_positions[user_positions.len() - drop];
        self.dialogue.truncate(cutoff);
        drop
    }

    /// The most recent assistant message, for `/copy`.
    pub fn last_assistant(&self) -> Option<&str> {
        self.dialogue
            .iter()
            .rev()
            .find(|entry| !entry.from_user)
            .map(|entry| entry.text.as_str())
    }

    /// Render the dialogue as markdown: `## user` / `## assistant`
    /// sections with the raw message text.
    pub fn export_markdown(&self) -> String {
        let mut out = String::from("# WaveCode session\n\n");
        out.push_str(&format!("- model: {}\n", self.model_name));
        out.push_str(&format!("- cwd: {}\n", self.cwd.display()));
        if let Some(branch) = &self.git_branch {
            out.push_str(&format!("- branch: {branch}\n"));
        }
        out.push('\n');
        for entry in &self.dialogue {
            out.push_str(if entry.from_user {
                "## user\n\n"
            } else {
                "## assistant\n\n"
            });
            out.push_str(&entry.text);
            out.push_str("\n\n");
        }
        out
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

    #[test]
    fn last_assistant_skips_user_messages() {
        let mut state = AppState::new(
            "m".to_string(),
            PathBuf::from("."),
            "guarded".to_string(),
            Vec::new(),
        );
        assert!(state.last_assistant().is_none());
        state.push_dialogue(true, "question");
        state.push_dialogue(false, "first answer");
        state.push_dialogue(true, "follow-up");
        state.push_dialogue(false, "second answer");
        assert_eq!(state.last_assistant(), Some("second answer"));
    }

    #[test]
    fn rewind_dialogue_drops_whole_user_turns() {
        let mut state = AppState::new(
            "m".to_string(),
            PathBuf::from("."),
            "guarded".to_string(),
            Vec::new(),
        );
        state.push_dialogue(true, "q1");
        state.push_dialogue(false, "a1");
        state.push_dialogue(true, "q2");
        state.push_dialogue(false, "a2");
        state.push_dialogue(true, "q3");
        state.push_dialogue(false, "a3");
        // Dropping two turns removes q2/a2 and q3/a3, keeps turn one.
        let removed = state.rewind_dialogue(2);
        assert_eq!(removed, 2);
        let texts: Vec<&str> = state.dialogue.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["q1", "a1"]);
        // Dropping more turns than exist removes what is there.
        assert_eq!(state.rewind_dialogue(5), 1);
        assert!(state.dialogue.is_empty());
        // Nothing to drop reports zero.
        assert_eq!(state.rewind_dialogue(1), 0);
    }

    #[test]
    fn export_markdown_has_roles_and_header() {
        let mut state = AppState::new(
            "test-model".to_string(),
            PathBuf::from("/tmp"),
            "guarded".to_string(),
            Vec::new(),
        );
        state.git_branch = Some("main".to_string());
        state.push_dialogue(true, "hello");
        state.push_dialogue(false, "hi there");
        let md = state.export_markdown();
        assert!(md.starts_with("# WaveCode session"), "{md}");
        assert!(md.contains("model: test-model"), "{md}");
        assert!(md.contains("branch: main"), "{md}");
        assert!(md.contains("## user\n\nhello"), "{md}");
        assert!(md.contains("## assistant\n\nhi there"), "{md}");
    }
}
