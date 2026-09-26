/*!
 * @file ProtocolVocabulary
 * @description Shared frontend-protocol vocabulary: permission modes and approval kinds.
 *
 * Responsibilities:
 * - Own the `PermissionMode` wire strings shared by config, sandbox, and frontends.
 * - Own the `ApprovalKind` display-routing enum shared by sandbox and frontends.
 * - Stay dependency-free vocabulary: no requests, events, or behavior.
 *
 * This module must not depend on: any workspace crate. It is pure data.
 * The live frontend request/event surface (`Submission` / `Op` / `Event` /
 * `EventMsg`) lives in exactly one place: `wavecode-wire`. Do not
 * reintroduce parallel copies here; a second source of truth drifts.
 */

//! wavecode-protocol: shared frontend-protocol vocabulary.
//!
//! This crate holds the small enums every layer names identically
//! (`PermissionMode`, `ApprovalKind`) so config, sandbox, actor, and
//! frontends cannot drift apart. It deliberately contains no request or
//! event types: the live `Submission { id, op }` / `Event { id, msg }`
//! surface lives in `wavecode-wire`, the single source of truth for
//! frontend communication.

use serde::{Deserialize, Serialize};

/// Permission mode (wire strings match config's `permission_mode`).
///
/// Lives in protocol rather than sandbox: it is the wire type of
/// `SetPermissionMode`, same as `StopReason`; sandbox only holds the
/// decision logic and depends on this type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum PermissionMode {
    /// Read-only exploration: only read-only tools are usable; everything
    /// else is denied straight back into the model. The model is nudged
    /// toward proposing a plan but may also simply answer.
    #[serde(rename = "plan")]
    Plan,
    /// Everything except dangerous operations flows through; command
    /// execution and destructive tools need per-call approval (hits on
    /// allow rules skip approval).
    #[serde(rename = "auto")]
    Auto,
    /// Approve everything (deny rules still apply). User-configured
    /// denylist entries (the wave denylist) are enforced as deny rules,
    /// so a banned command is refused outright without a prompt.
    #[serde(rename = "wave")]
    Wave,
}

impl PermissionMode {
    /// Parses a mode string from config / the frontend (same names as the
    /// serde wire form). Legacy names map onto their successor so old
    /// config files keep loading: `guarded` / `default` / `acceptEdits` ->
    /// Auto, `bypassPermissions` / `yolo` -> Wave. Note that `auto` was
    /// redefined: it used to mean "approve everything" (now `wave`) and
    /// now means "ask only on dangerous operations".
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "plan" => Some(Self::Plan),
            "auto" | "guarded" | "default" | "acceptEdits" => Some(Self::Auto),
            "wave" | "bypassPermissions" | "yolo" => Some(Self::Wave),
            _ => None,
        }
    }

    /// Wire string (shared by config and display).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::Wave => "wave",
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Approval kind (the approval request's `kind`), letting the frontend pick a display shape.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApprovalKind {
    /// Shell command execution.
    Exec,
    /// File write / edit.
    Write,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_mode_wire_form_parses_and_displays() {
        // Lock the PermissionMode wire form (matches the config `permission_mode` strings).
        let mode_cases: [(PermissionMode, &str); 3] = [
            (PermissionMode::Plan, "plan"),
            (PermissionMode::Auto, "auto"),
            (PermissionMode::Wave, "wave"),
        ];
        for (mode, tag) in mode_cases {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!(r#""{tag}""#), "PermissionMode tag drifted");
            assert_eq!(PermissionMode::parse(tag), Some(mode));
            assert_eq!(mode.to_string(), tag);
        }
        assert_eq!(PermissionMode::parse("Plan"), None);
    }

    #[test]
    fn legacy_mode_names_migrate_onto_their_successors() {
        // Pre-rename config files keep loading: old names alias onto the
        // mode that inherited their behavior.
        assert_eq!(PermissionMode::parse("guarded"), Some(PermissionMode::Auto));
        assert_eq!(PermissionMode::parse("default"), Some(PermissionMode::Auto));
        assert_eq!(
            PermissionMode::parse("acceptEdits"),
            Some(PermissionMode::Auto)
        );
        assert_eq!(
            PermissionMode::parse("bypassPermissions"),
            Some(PermissionMode::Wave)
        );
        assert_eq!(PermissionMode::parse("yolo"), Some(PermissionMode::Wave));
    }

    #[test]
    fn approval_kind_wire_form_locked() {
        let kind_cases: [(ApprovalKind, &str); 2] =
            [(ApprovalKind::Exec, "exec"), (ApprovalKind::Write, "write")];
        for (kind, tag) in kind_cases {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(json, format!(r#""{tag}""#), "ApprovalKind tag drifted");
        }
    }
}
