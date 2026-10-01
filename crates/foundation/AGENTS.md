# foundation/ agent rules

The crate map lives in [README.md](README.md). Vocabulary crates have
their own rules: [protocol/AGENTS.md](protocol/AGENTS.md),
[wire/AGENTS.md](wire/AGENTS.md).

## Boundaries

- `auth/`, `config/`, and `llm/` depend only on external crates.
- `wavecode-auth` is unwired. Provider credentials resolve through
  `wavecode-config`'s `env_key` path. Do not cite `wavecode-auth` as
  a product feature. Wiring it updates `docs/architecture.md`.

## Config

- This layer parses TOML. It does not validate hook event points, the
  MCP stdio-versus-http either-or, or permission-rule syntax. Those
  checks stay in assembly and in `wavecode-sandbox`.
- API-key resolution prefers the env var named by `env_key`, then
  the inline `api_key`. An empty or whitespace-only env value falls
  through to the inline key. A blank inline key is
  `ConfigError::MissingApiKey`.
- `home_dir()` reads `USERPROFILE`, then `HOME`. Loading does not
  fall back to a relative path.
- The `[permissions]` table is read from the user-level config file
  only. Do not load it from a repository config.

## LLM

- Callers sample through the `ChatModel` trait. Shared call paths do
  not branch on a provider name. Vendor-specific request shapes stay
  in `anthropic.rs`, `openai.rs`, and `responses.rs`.
- Config values win over `ModelCapabilities`. An unknown model name
  returns `None`.
- `Usage::input_tokens` is the full request size on every wire.
- Auth failures, quota failures, and `PromptTooLong` fail fast.
  Retries apply to transient failures only.
- `Credential`'s `Debug` prints the `REDACTED` placeholder (`"***"`)
  for key material.
- `ProviderConfig`'s `Debug` prints `api_key` as `"***"` when it is
  set, and prints `env_key` as the variable name.
