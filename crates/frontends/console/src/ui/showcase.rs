//! Visual review harness (test-only): renders real UI frames per
//! theme and dumps the raw ANSI rows to the temp directory for
//! inspection. Not a behavioral test — it exists so the frame content
//! a user sees can be reviewed outside the terminal.

use super::*;
use crate::ui::test_support::{NullStatus, TestLink};
use std::path::PathBuf;
use std::sync::Arc;

fn out_dir() -> PathBuf {
    // nosemgrep: Semgrep_rust.lang.security.temp-dir.temp-dir
    let dir = std::env::temp_dir().join("wavecode-showcase");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub(super) fn showcase_ui() -> ConsoleUi {
    let mut ui = ConsoleUi::new(
        Box::new(TestLink::new()),
        &UiContext {
            model_name: "MiniMax-M3".to_string(),
            provider_id: "minimax".to_string(),
            thinking_effort: None,
            thinking_levels: vec!["off".to_string(), "low".to_string(), "high".to_string()],
            cwd: PathBuf::from("~/wave-code"),
            permission_mode: "auto".to_string(),
            skill_names: Vec::new(),
            mcp_servers: vec!["fs".to_string()],
            memory_files: Vec::new(),
            status: Arc::new(NullStatus),
            session_id: "session-0001".to_string(),
            session_title: None,
            model_entries: vec![
                ModelEntryView {
                    label: "MiniMax-M3".to_string(),
                    provider: "minimax".to_string(),
                    model: "MiniMax-M3".to_string(),
                    effort: None,
                },
                ModelEntryView {
                    label: "GLM-5.3".to_string(),
                    provider: "zhipu".to_string(),
                    model: "glm-5.3".to_string(),
                    effort: Some("high".to_string()),
                },
            ],
            home: None,
            update_notice: None,
            redactor: None,
        },
        "0.1.0",
    );
    ui.settings = crate::settings::SharedSettings::without_persistence(Default::default());
    ui
}

fn dump(name: &str, lines: &[String]) {
    let body = lines.join("\n");
    std::fs::write(out_dir().join(format!("{name}.txt")), body).unwrap();
}

const ASSISTANT_MARKDOWN: &str = "\
## Plan\n\
\n\
1. Split the tokenizer into two passes\n\
2. Teach `parse_expr` about **precedence**\n\
\n\
- read `src/parser.rs` first\n\
- see [the RFC](https://example.com/rfc) for context\n\
\n\
> note: the grammar is LL(1) today\n\
\n\
| stage | cost |\n\
| --- | --- |\n\
| lex | 12ms |\n\
| parse | 30ms |\n\
\n\
```diff\n\
--- a/parser.rs\n\
+++ b/parser.rs\n\
@@ -1,3 +1,3 @@\n\
-fn old_expr() {}\n\
+fn new_expr() {}\n\
```\n\
\n\
```rust\n\
fn parse_expr(input: &str) -> Expr {\n\
    Expr::new(input)\n\
}\n\
```\n";

#[test]
#[ignore = "visual-review harness: dumps ANSI frames to the temp dir; run with --ignored"]
fn dump_visual_showcase_frames() {
    let themes = [
        ("dark", theme::Theme::dark()),
        ("deepwave", theme::Theme::deepwave()),
        ("light", theme::Theme::light()),
    ];
    for (name, theme) in themes {
        theme::set(theme);

        // Transcript: user input, tool card, thinking, rich assistant
        // markdown, statuses.
        let mut ui = showcase_ui();
        ui.user_submit("help me refactor the parser module");
        ui.handle_wire_event(&EventMsg::ToolCallBegin {
            call_id: "t1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({ "path": "src/parser.rs" }),
        });
        ui.handle_wire_event(&EventMsg::ToolCallEnd {
            call_id: "t1".to_string(),
            is_error: false,
            output: Some(wavecode_wire::ToolCallPreview {
                text: "1285 lines".to_string(),
                truncated: false,
            }),
            outcome: wavecode_wire::ToolOutcome::Executed,
            duration_ms: 34,
        });
        ui.handle_wire_event(&EventMsg::AgentThinkingDelta {
            text: "the tokenizer is single-pass today; splitting it needs care".to_string(),
        });
        ui.handle_wire_event(&EventMsg::AgentMessageDelta {
            text: ASSISTANT_MARKDOWN.to_string(),
        });
        ui.handle_wire_event(&EventMsg::AgentMessageComplete {
            text: ASSISTANT_MARKDOWN.to_string(),
        });
        ui.handle_wire_event(&EventMsg::TurnCompleted { interrupted: false });
        ui.handle_wire_event(&EventMsg::Warning {
            message: "warning: tool round limit reached (256)".to_string(),
        });
        ui.handle_wire_event(&EventMsg::Error {
            message: "connection lost".to_string(),
            recoverable: true,
            code: None,
        });
        ui.handle_wire_event(&EventMsg::TokenCount {
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            input_tokens: 3_000,
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            output_tokens: 500,
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            cache_read_tokens: 0,
            // nosemgrep: Semgrep_codacy.yaml.security.hard-coded-tokens
            cache_creation_tokens: 0,
            context_window: Some(195_000),
            context_used: Some(3_500),
        });
        let _ = ui.frame(80, 24);
        dump(&format!("{name}-transcript"), &ui.frame(80, 24));

        // Dialogs: theme, effort, permissions, help, title prompt,
        // model picker, settings.
        let mut ui = showcase_ui();
        ui.user_submit("/theme");
        dump(&format!("{name}-dialog-theme"), &ui.frame(80, 24));

        let mut ui = showcase_ui();
        ui.user_submit("/effort");
        dump(&format!("{name}-dialog-effort"), &ui.frame(80, 24));

        let mut ui = showcase_ui();
        ui.user_submit("/permissions");
        dump(&format!("{name}-dialog-permissions"), &ui.frame(80, 24));

        let mut ui = showcase_ui();
        ui.user_submit("/help");
        dump(&format!("{name}-dialog-help"), &ui.frame(80, 24));

        let mut ui = showcase_ui();
        ui.user_submit("/title");
        dump(&format!("{name}-dialog-title"), &ui.frame(80, 24));

        let mut ui = showcase_ui();
        ui.user_submit("/model");
        dump(&format!("{name}-dialog-model"), &ui.frame(80, 24));

        let mut ui = showcase_ui();
        ui.user_submit("/settings");
        dump(&format!("{name}-dialog-settings"), &ui.frame(80, 24));

        // Autocomplete popup on the editor.
        let mut ui = showcase_ui();
        for ch in "/th".chars() {
            ui.handle_key(KeyEvent::plain(tui_engine::keys::Key::Char(ch)));
        }
        dump(&format!("{name}-autocomplete"), &ui.frame(80, 24));
    }
}
