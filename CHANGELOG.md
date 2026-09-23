# Changelog

All notable changes to `wavecode` are documented here. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the
project does not cut releases on a fixed cadence, so entries
accumulate under `Unreleased` until a `v*` tag publishes them.

## [Unreleased]

### Added
- Session system: `--session` resume, `--continue`, `/sessions`,
  `/fork`, `/title`, `/new`, text-level turn journal for replay.
- Tool surface: background tasks, goal tracking, todo panels,
  `ask_user`, file snapshots with rewind, memory, skills, plan mode.
- Security: deny-first path sandbox with OS-level confinement on all
  three platforms, permission modes (`plan` / `auto` / `wave`),
  persisted "always allow" grants with a `grants` audit command,
  sensitive-file prompts, SSRF-guarded web fetch.
- Headless: `exec --json` streaming wire events with a stdin
  approval channel, ACP mode negotiation, app server (REST + SSE),
  TypeScript SDK.
- Console UI: wave-themed rendering, syntax highlighting, window
  title and tab progress, kill-ring/yank-pop, paste-burst protection,
  external status-line command (Claude Code contract), custom JSON
  themes, `/undo` rewind picker, `/editor` (Ctrl+G), `/reload`.
- Reliability: write-ahead history journal, checkpoint manifests,
  bounded LLM/MCP transports (no hang-forever paths), panic
  isolation for tool children, MCP reconnect/heal.
- Engineering: `doctor` diagnostics, `update` release check,
  `eval` task harness, offline metrics reporting, binary release
  pipeline with sha256-verified archives.

### Fixed
- Journal tear recovery: torn writes and multi-byte UTF-8 cuts no
  longer read as an empty history.
- Windows: snapshot manifests cannot escape the restore root through
  backslash or drive-prefix path semantics; pty sessions no longer
  leak shell processes on setup failure.
- Correctness: permission-mode typos are rejected instead of
  silently widening to `auto`; oversized `--image` inputs are
  rejected before loading into memory.
