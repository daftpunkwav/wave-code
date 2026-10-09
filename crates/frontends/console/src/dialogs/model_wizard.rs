//! Step-at-a-time model builder behind `/provider add`, plus its
//! helper components (step order, size choices, multiselects).

use super::Answer;
use crate::theme::{self, Token};
use tui_engine::border;
use tui_engine::keys::{Key, KeyEvent};

/// One step of the model wizard, in fill order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WizardStep {
    ProviderName,
    ApiKind,
    BaseUrl,
    ApiKey,
    ModelId,
    Alias,
    ContextWindow,
    MaxOutput,
    Thinking,
    InputMods,
    OutputMods,
    Review,
}

const WIZARD_ORDER: [WizardStep; 12] = [
    WizardStep::ProviderName,
    WizardStep::ApiKind,
    WizardStep::BaseUrl,
    WizardStep::ApiKey,
    WizardStep::ModelId,
    WizardStep::Alias,
    WizardStep::ContextWindow,
    WizardStep::MaxOutput,
    WizardStep::Thinking,
    WizardStep::InputMods,
    WizardStep::OutputMods,
    WizardStep::Review,
];

/// Context presets offered before free-form entry. `k` and `M` are the
/// binary units catalogs quote (256k = 262144).
const CONTEXT_PRESETS: [&str; 3] = ["256k", "500k", "1M"];
const MAX_OUTPUT_PRESETS: [&str; 2] = ["128k", "64k"];
/// Reasoning-effort presets every model can tick; customs append.
pub const THINKING_PRESETS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// Modality presets.
const INPUT_MOD_PRESETS: [&str; 4] = ["text", "image", "audio", "video"];
const OUTPUT_MOD_PRESETS: [&str; 2] = ["text", "image"];

/// Parse a size cell: a bare token count, or `Nk` / `NM` in binary
/// units. `None` when the cell is blank or malformed.
fn parse_size(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if let Ok(count) = lower.parse::<u64>() {
        return Some(count);
    }
    // checked_mul: a huge custom value must fall through to the
    // validation error, not panic (debug) or wrap (release).
    if let Some(head) = lower.strip_suffix('k') {
        return head
            .trim()
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(1024));
    }
    if let Some(head) = lower.strip_suffix('m') {
        return head
            .trim()
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(1024 * 1024));
    }
    None
}

/// Where one key leaves the current wizard step.
enum Nav {
    Stay,
    Prev,
    Next,
    /// Esc at a component-owned step (outside its custom-entry field):
    /// leave the wizard like the text steps do.
    Exit,
}

/// A checkable option list with a cursor and inline custom-entry input
/// (typed after `+`, committed with Enter). Space toggles.
struct MultiSelect {
    options: Vec<String>,
    checked: Vec<bool>,
    cursor: usize,
    /// Some(_): typing a custom option name into this buffer.
    custom: Option<String>,
}

impl MultiSelect {
    fn new(presets: &[&str], prechecked: &[&str]) -> Self {
        let options: Vec<String> = presets.iter().map(|s| s.to_string()).collect();
        let checked = options
            .iter()
            .map(|option| prechecked.contains(&option.as_str()))
            .collect();
        Self {
            options,
            checked,
            cursor: 0,
            custom: None,
        }
    }

    fn selected(&self) -> Vec<String> {
        self.options
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(option, _)| option.clone())
            .collect()
    }

    /// One key while this list owns the step.
    fn handle_key(&mut self, key: &Key, mods: &tui_engine::keys::Mods) -> Nav {
        if let Some(buffer) = &mut self.custom {
            match key {
                Key::Enter => {
                    let name = buffer.trim().to_string();
                    self.custom = None;
                    if !name.is_empty() && !self.options.iter().any(|o| o == &name) {
                        self.options.push(name);
                        self.checked.push(true);
                        self.cursor = self.options.len() - 1;
                    }
                }
                Key::Backspace => {
                    buffer.pop();
                }
                Key::Char(c) if !mods.ctrl && !mods.alt => buffer.push(*c),
                Key::Esc => self.custom = None,
                _ => {}
            }
            return Nav::Stay;
        }
        match key {
            Key::Up => self.cursor = self.cursor.saturating_sub(1),
            Key::Down => self.cursor = (self.cursor + 1).min(self.options.len()),
            Key::Char(' ') => {
                if let Some(checked) = self.checked.get_mut(self.cursor) {
                    *checked = !*checked;
                }
            }
            Key::Char('+') => self.custom = Some(String::new()),
            Key::Esc => return Nav::Exit,
            Key::Backspace => return Nav::Prev,
            Key::Enter => return Nav::Next,
            _ => {}
        }
        Nav::Stay
    }

    fn render(&self, theme: &crate::theme::Theme, body: &mut Vec<String>) {
        for (index, option) in self.options.iter().enumerate() {
            let tick = if self.checked[index] { "[x]" } else { "[ ]" };
            let line = format!("{tick} {option}");
            if index == self.cursor {
                body.push(theme.bold(Token::TextStrong, &format!("❯ {line}")));
            } else {
                body.push(theme.paint(Token::Text, &format!("  {line}")));
            }
        }
        // The custom-entry row sits after the presets.
        match &self.custom {
            Some(buffer) => {
                body.push(theme.bold(Token::Accent, &format!("❯ + custom: {buffer}▏")));
            }
            None => {
                let line = "+ add custom";
                if self.cursor == self.options.len() {
                    body.push(theme.bold(Token::TextStrong, &format!("❯ {line}")));
                } else {
                    body.push(theme.paint(Token::TextDim, &format!("  {line}")));
                }
            }
        }
    }
}

/// A pick-one list with presets and a trailing free-form entry
/// (`256k` / `500k` / `1M` / custom).
struct SizeChoice {
    presets: Vec<String>,
    cursor: usize,
    /// Some(text): the custom cell being typed.
    custom: Option<String>,
    chosen_custom: Option<String>,
}

impl SizeChoice {
    fn new(presets: &[&str]) -> Self {
        Self {
            presets: presets.iter().map(|s| s.to_string()).collect(),
            cursor: 0,
            custom: None,
            chosen_custom: None,
        }
    }

    fn handle_key(&mut self, key: &Key, mods: &tui_engine::keys::Mods) -> Nav {
        if let Some(buffer) = &mut self.custom {
            match key {
                Key::Enter => {
                    let text = buffer.trim().to_string();
                    if parse_size(&text).is_none() {
                        return Nav::Stay; // invalid input keeps the field
                    }
                    self.chosen_custom = Some(text);
                    self.custom = None;
                    return Nav::Next;
                }
                Key::Backspace => {
                    buffer.pop();
                }
                Key::Char(c) if !mods.ctrl && !mods.alt => buffer.push(*c),
                Key::Esc => self.custom = None,
                _ => {}
            }
            return Nav::Stay;
        }
        match key {
            Key::Up => self.cursor = self.cursor.saturating_sub(1),
            Key::Down => self.cursor = (self.cursor + 1).min(self.presets.len()),
            Key::Esc => return Nav::Exit,
            Key::Enter => {
                if self.cursor == self.presets.len() {
                    self.custom = Some(String::new());
                } else {
                    self.chosen_custom = None;
                    return Nav::Next;
                }
            }
            _ => {}
        }
        Nav::Stay
    }

    /// The raw cell (a preset string or the typed custom value).
    fn value(&self) -> String {
        if let Some(custom) = &self.chosen_custom {
            return custom.clone();
        }
        self.presets
            .get(self.cursor)
            .cloned()
            .unwrap_or_else(|| self.presets.first().cloned().unwrap_or_default())
    }

    fn render(&self, theme: &crate::theme::Theme, body: &mut Vec<String>) {
        for (index, preset) in self.presets.iter().enumerate() {
            let line = preset.clone();
            if index == self.cursor && self.custom.is_none() {
                body.push(theme.bold(Token::TextStrong, &format!("❯ {line}")));
            } else {
                body.push(theme.paint(Token::Text, &format!("  {line}")));
            }
        }
        match &self.custom {
            Some(buffer) => {
                body.push(theme.bold(Token::Accent, &format!("❯ custom: {buffer}▏")));
            }
            None => {
                let line = "custom…";
                if self.cursor == self.presets.len() {
                    body.push(theme.bold(Token::TextStrong, &format!("❯ {line}")));
                } else {
                    body.push(theme.paint(Token::TextDim, &format!("  {line}")));
                }
            }
        }
    }
}

/// The step-at-a-time model builder behind `/provider add`: provider
/// fields first, then per-model fields, with a review step that can
/// loop back to add another model under the same provider. All
/// accumulated entries submit together.
pub struct ModelWizardDialog {
    pub(super) title: String,
    pub(super) step: WizardStep,
    // Provider-level fields survive the "add another model" loop.
    pub(super) provider: String,
    pub(super) api: usize,
    pub(super) base_url: String,
    pub(super) api_key: String,
    // Model-level fields reset per model.
    pub(super) model_id: String,
    alias: String,
    context: SizeChoice,
    max_output: SizeChoice,
    thinking: MultiSelect,
    input_mods: MultiSelect,
    output_mods: MultiSelect,
    pub(super) entries: Vec<(String, wavecode_config::ModelSpec)>,
    pub(super) error: Option<String>,
}

pub const API_KINDS: [&str; 3] = ["anthropic-messages", "openai-chat", "openai-responses"];

impl ModelWizardDialog {
    /// A wizard over an optional provider preset (picked from the
    /// provider list): the name, dialect, endpoint, and key env seed
    /// the first four steps.
    pub fn new(preset: Option<crate::ui::ProviderPreset>) -> Self {
        let (provider, api, base_url, api_key) = preset
            .map(|preset| {
                let api = API_KINDS
                    .iter()
                    .position(|kind| *kind == preset.api)
                    .unwrap_or(0);
                (
                    preset.provider,
                    api,
                    preset.base_url,
                    preset.api_key_env.unwrap_or_default(),
                )
            })
            .unwrap_or_default();
        Self {
            title: "Model wizard".to_string(),
            step: WIZARD_ORDER[0],
            provider,
            api,
            base_url,
            api_key,
            model_id: String::new(),
            alias: String::new(),
            context: SizeChoice::new(&CONTEXT_PRESETS),
            max_output: SizeChoice::new(&MAX_OUTPUT_PRESETS),
            thinking: MultiSelect::new(&THINKING_PRESETS, &[]),
            input_mods: MultiSelect::new(&INPUT_MOD_PRESETS, &["text"]),
            output_mods: MultiSelect::new(&OUTPUT_MOD_PRESETS, &["text"]),
            entries: Vec::new(),
            error: None,
        }
    }

    fn step_title(&self) -> &'static str {
        match self.step {
            WizardStep::ProviderName => "Provider name",
            WizardStep::ApiKind => "API format",
            WizardStep::BaseUrl => "Base URL",
            WizardStep::ApiKey => "API key (env:NAME stores the variable name, blank skips)",
            WizardStep::ModelId => "Model name (the wire id)",
            WizardStep::Alias => "Display name (the /model alias)",
            WizardStep::ContextWindow => "Context window",
            WizardStep::MaxOutput => "Max output tokens",
            WizardStep::Thinking => "Thinking levels (space toggles, + adds custom)",
            WizardStep::InputMods => "Input modalities",
            WizardStep::OutputMods => "Output modalities",
            WizardStep::Review => "Review — ↵ stages & continues, s saves all",
        }
    }

    /// Compose the current model's spec; `Err` names the field problem.
    fn resolve_model(&self) -> Result<(String, wavecode_config::ModelSpec), String> {
        if self.model_id.trim().is_empty() {
            return Err("model name is required".to_string());
        }
        if self.alias.trim().is_empty() {
            return Err("display name is required".to_string());
        }
        if self.provider.trim().is_empty() {
            return Err("provider is required".to_string());
        }
        if self.base_url.trim().is_empty() {
            return Err("base url is required".to_string());
        }
        let context_window = parse_size(&self.context.value())
            .ok_or_else(|| "context window must be a size (256k / 1M / digits)".to_string())?;
        let max_output = parse_size(&self.max_output.value())
            .and_then(|size| u32::try_from(size).ok())
            .ok_or_else(|| "max output must be a size (64k / 128k / digits)".to_string())?;
        let kind = match API_KINDS[self.api] {
            "openai-chat" => wavecode_config::ApiKind::OpenaiChat,
            "openai-responses" => wavecode_config::ApiKind::OpenaiResponses,
            _ => wavecode_config::ApiKind::AnthropicMessages,
        };
        let variants = self.thinking.selected();
        let reasoning = wavecode_config::ReasoningSpec {
            enabled: !variants.is_empty(),
            default: variants.first().cloned(),
            variants,
        };
        let key = self.api_key.trim();
        let (api_key_env, api_key) = if let Some(env) = key.strip_prefix("env:") {
            ((!env.is_empty()).then(|| env.to_string()), None)
        } else {
            (None, (!key.is_empty()).then(|| key.to_string()))
        };
        let spec = wavecode_config::ModelSpec {
            provider: self.provider.trim().to_string(),
            model: self.model_id.trim().to_string(),
            kind,
            base_url: self.base_url.trim().to_string(),
            api_key_env,
            api_key,
            context_window: Some(context_window),
            max_output: Some(max_output),
            reasoning,
            modalities: wavecode_config::ModalitiesSpec {
                input: self.input_mods.selected(),
                output: self.output_mods.selected(),
            },
        };
        Ok((self.alias.trim().to_string(), spec))
    }

    /// Reset the per-model fields for the next entry on this provider.
    fn reset_model_fields(&mut self) {
        self.model_id.clear();
        self.alias.clear();
        self.context = SizeChoice::new(&CONTEXT_PRESETS);
        self.max_output = SizeChoice::new(&MAX_OUTPUT_PRESETS);
        self.thinking = MultiSelect::new(&THINKING_PRESETS, &[]);
        self.input_mods = MultiSelect::new(&INPUT_MOD_PRESETS, &["text"]);
        self.output_mods = MultiSelect::new(&OUTPUT_MOD_PRESETS, &["text"]);
    }

    /// The Esc outcome: staged entries submit together, a bare wizard
    /// dismisses (the wizard never throws away confirmed work).
    fn esc_answer(&mut self) -> Option<Answer> {
        if self.entries.is_empty() {
            return Some(Answer::Dismissed);
        }
        Some(Answer::ModelForm {
            entries: std::mem::take(&mut self.entries),
        })
    }

    pub(super) fn handle_key(&mut self, event: KeyEvent) -> Option<Answer> {
        let index = WIZARD_ORDER
            .iter()
            .position(|step| *step == self.step)
            .unwrap_or(0);
        // Multiselect steps own their keys first.
        let nav = match self.step {
            WizardStep::Thinking => self.thinking.handle_key(&event.key, &event.mods),
            WizardStep::InputMods => self.input_mods.handle_key(&event.key, &event.mods),
            WizardStep::OutputMods => self.output_mods.handle_key(&event.key, &event.mods),
            WizardStep::ContextWindow => self.context.handle_key(&event.key, &event.mods),
            WizardStep::MaxOutput => self.max_output.handle_key(&event.key, &event.mods),
            _ => Nav::Stay,
        };
        match nav {
            Nav::Prev => {
                if index > 0 {
                    self.step = WIZARD_ORDER[index - 1];
                }
                return None;
            }
            Nav::Next => {
                if index + 1 < WIZARD_ORDER.len() {
                    self.step = WIZARD_ORDER[index + 1];
                }
                return None;
            }
            Nav::Exit => return self.esc_answer(),
            Nav::Stay => {
                // The choice and multiselect steps own the keyboard:
                // their component advances them (Nav::Next), and a Stay
                // here — e.g. the Enter that opens a custom-entry
                // field — must not also fall through to the generic
                // step advance. Text and api steps stay fallible on
                // purpose: their Enter rides the generic advance below.
                if matches!(
                    self.step,
                    WizardStep::ContextWindow
                        | WizardStep::MaxOutput
                        | WizardStep::Thinking
                        | WizardStep::InputMods
                        | WizardStep::OutputMods
                ) {
                    return None;
                }
            }
        }
        match event.key {
            Key::Char('s')
                if !event.mods.ctrl && !event.mods.alt && self.step == WizardStep::Review =>
            {
                // Save everything: stage the current model when it
                // resolves, then submit all staged entries. A broken
                // current model with nothing staged keeps the wizard
                // open with the error; staged work survives a broken
                // draft.
                match self.resolve_model() {
                    Ok((alias, spec)) => self.entries.push((alias, spec)),
                    Err(message) if self.entries.is_empty() => {
                        self.error = Some(message);
                        return None;
                    }
                    Err(_) => {}
                }
                return Some(Answer::ModelForm {
                    entries: std::mem::take(&mut self.entries),
                });
            }
            Key::Esc => {
                return self.esc_answer();
            }
            Key::Backspace => {
                let active_text = match self.step {
                    WizardStep::ProviderName => &mut self.provider,
                    WizardStep::BaseUrl => &mut self.base_url,
                    WizardStep::ApiKey => &mut self.api_key,
                    WizardStep::ModelId => &mut self.model_id,
                    WizardStep::Alias => &mut self.alias,
                    _ => {
                        if index > 0 {
                            self.step = WIZARD_ORDER[index - 1];
                        }
                        return None;
                    }
                };
                if active_text.pop().is_none() && index > 0 {
                    self.step = WIZARD_ORDER[index - 1];
                }
                self.error = None;
            }
            Key::Left | Key::Right | Key::Up | Key::Down if self.step == WizardStep::ApiKind => {
                // The vertical list also answers ↑/↓ like every other
                // step; ←/→ keep working as the wrap-around shortcut.
                let down = matches!(event.key, Key::Right | Key::Down);
                let step = if down { 1 } else { API_KINDS.len() - 1 };
                self.api = (self.api + step) % API_KINDS.len();
            }
            Key::Char(c) if !event.mods.ctrl && !event.mods.alt => {
                let field = match self.step {
                    WizardStep::ProviderName => Some(&mut self.provider),
                    WizardStep::BaseUrl => Some(&mut self.base_url),
                    WizardStep::ApiKey => Some(&mut self.api_key),
                    WizardStep::ModelId => Some(&mut self.model_id),
                    WizardStep::Alias => Some(&mut self.alias),
                    _ => None,
                };
                if let Some(field) = field {
                    field.push(c);
                }
                self.error = None;
            }
            Key::Enter if self.step == WizardStep::Review => {
                // Stage the model and reset the per-model fields: the
                // review page doubles as the add-another-model loop.
                match self.resolve_model() {
                    Ok((alias, spec)) => {
                        self.entries.push((alias, spec));
                        self.reset_model_fields();
                        // The loop continues at the model steps: the
                        // provider fields carry over untouched.
                        self.step = WizardStep::ModelId;
                        self.error = None;
                    }
                    Err(message) => self.error = Some(message),
                }
            }
            Key::Enter if index + 1 < WIZARD_ORDER.len() => {
                self.step = WIZARD_ORDER[index + 1];
            }
            _ => {}
        }
        None
    }

    pub(super) fn render(&mut self, columns: usize) -> Vec<String> {
        let theme = theme::current();
        let index = WIZARD_ORDER
            .iter()
            .position(|step| *step == self.step)
            .unwrap_or(0);
        let mut body = vec![theme.paint(
            Token::TextDim,
            &format!(
                "provider {} · {} · step {}/{}",
                if self.provider.is_empty() {
                    "(new)"
                } else {
                    &self.provider
                },
                API_KINDS[self.api],
                index + 1,
                WIZARD_ORDER.len()
            ),
        )];
        body.push(theme.bold(Token::Text, self.step_title()));
        body.push(String::new());
        let text = |value: &str| {
            let mut shown = value.to_string();
            if !value.is_empty() && self.step == WizardStep::ApiKey {
                shown = "*".repeat(value.len().min(24));
            }
            shown
        };
        match self.step {
            WizardStep::ProviderName => {
                body.push(theme.bold(Token::TextStrong, &format!("{}▏", text(&self.provider))));
            }
            WizardStep::ApiKind => {
                for (position, kind) in API_KINDS.iter().enumerate() {
                    if position == self.api {
                        body.push(theme.bold(Token::TextStrong, &format!("❯ {kind}")));
                    } else {
                        body.push(theme.paint(Token::TextDim, &format!("  {kind}")));
                    }
                }
            }
            WizardStep::BaseUrl | WizardStep::ApiKey => {
                let value = if self.step == WizardStep::BaseUrl {
                    &self.base_url
                } else {
                    &self.api_key
                };
                body.push(theme.bold(Token::TextStrong, &format!("{}▏", text(value))));
            }
            WizardStep::ModelId | WizardStep::Alias => {
                let value = if self.step == WizardStep::ModelId {
                    &self.model_id
                } else {
                    &self.alias
                };
                body.push(theme.bold(Token::TextStrong, &format!("{}▏", text(value))));
            }
            WizardStep::ContextWindow => self.context.render(&theme, &mut body),
            WizardStep::MaxOutput => self.max_output.render(&theme, &mut body),
            WizardStep::Thinking => self.thinking.render(&theme, &mut body),
            WizardStep::InputMods => self.input_mods.render(&theme, &mut body),
            WizardStep::OutputMods => self.output_mods.render(&theme, &mut body),
            WizardStep::Review => {
                let rows = [
                    ("provider", self.provider.clone()),
                    ("api", API_KINDS[self.api].to_string()),
                    ("base url", self.base_url.clone()),
                    (
                        "api key",
                        if self.api_key.starts_with("env:") {
                            format!("env {}", &self.api_key[4..])
                        } else if self.api_key.is_empty() {
                            "(none)".to_string()
                        } else {
                            "*".repeat(self.api_key.len().min(12))
                        },
                    ),
                    ("model", self.model_id.clone()),
                    ("alias", self.alias.clone()),
                    ("context", self.context.value()),
                    ("max output", self.max_output.value()),
                    ("thinking", self.thinking.selected().join(",")),
                    ("input", self.input_mods.selected().join(",")),
                    ("output", self.output_mods.selected().join(",")),
                ];
                for (label, value) in rows {
                    body.push(format!(
                        "{} {}",
                        theme.paint(Token::TextDim, &format!("{label:>10}:")),
                        theme.paint(Token::Text, &value)
                    ));
                }
                if !self.entries.is_empty() {
                    body.push(theme.paint(
                        Token::Success,
                        &format!("{} model(s) staged", self.entries.len()),
                    ));
                }
            }
        }
        if let Some(error) = &self.error {
            body.push(theme.paint(Token::Warning, error));
        }
        body.push(String::new());
        let hint = match self.step {
            WizardStep::Review => "↵ stage & continue · s save all · esc finish",
            WizardStep::Thinking | WizardStep::InputMods | WizardStep::OutputMods => {
                "↑/↓ move · space toggle · + custom · ↵ next · esc cancel"
            }
            WizardStep::ContextWindow | WizardStep::MaxOutput => {
                "↑/↓ pick · ↵ next (custom asks for a value) · esc cancel"
            }
            _ => "↵ next · backspace back · esc cancel",
        };
        body.push(theme.paint(Token::TextDim, hint));
        if self.step == WizardStep::ApiKind {
            // The hint tail is generic; this step adds its wrap-around
            // shortcut to the list navigation.
            body.push(theme.paint(Token::TextDim, "←/→ cycle · ↵ pick & next"));
        }
        border::frame(
            body,
            columns,
            theme.style(Token::BorderFocus),
            Some(self.title.clone()),
        )
    }
}
