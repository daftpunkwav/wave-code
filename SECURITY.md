# Security Policy

## Supported versions

Only the latest published release (and `main`) receives security
fixes. `wavecode` is a local CLI tool; there is no server component
and no backport cadence to older tags.

## Reporting a vulnerability

Use GitHub's private security advisories for
[daftpunkwav/wave-code](https://github.com/daftpunkwav/wave-code/security/advisories/new).
Please do not open a public issue for anything exploit-looking.

Include: the affected command surface (`exec` / `serve` / `acp` /
REPL), a reproduction (repo layout, config, invocation), and your
assessment of impact. You will get an acknowledgement within a few
days and a fix timeline once the report is triaged.

## Scope

In scope:

- Sandbox escapes: reads/writes/exec outside the workspace that the
  permission modes promise to confine, including Windows path
  semantics (backslashes, drive prefixes, junctions) and symlinks.
- Approval bypasses: paths where a tool runs without the prompted
  consent, or a deny/allow decision is misapplied.
- Credential handling: API keys or tokens leaking into session
  journals, exports, logs, error messages, or telemetry (there is no
  telemetry).
- Network surfaces: the local app server (auth, origin checks), MCP
  client/server transport, and SSRF in web fetch/search.
- Terminal integrity: untrusted text reaching the terminal as
  control sequences (escape injection, OSC abuse).
- Supply chain: the install/update path (asset verification,
  self-update swap).

Out of scope:

- Prompt injection making the model produce bad code within its
  granted permissions (an inherent LLM property; the sandbox is the
  boundary).
- Attacks requiring local malware already running as the user.
- Volumes/rate issues against external services.

## Hardening notes

- The path sandbox is deny-first with rules persisted per grant;
  `wavecode doctor` reports the effective confinement.
- Release archives and the self-update path verify sha256 checksums
  before install; a failed verification aborts without touching the
  running binary.
- Untrusted text is sanitized through one shared terminal gate before
  rendering; notifications ride tmux-safe wrappers.
