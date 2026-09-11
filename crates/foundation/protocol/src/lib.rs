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
 * `EventMsg`) lives in exactly one place: `operations-wire`. Do not
 * reintroduce parallel copies here; a second source of truth drifts.
 */

//! wavecode-protocol: shared frontend-protocol vocabulary.
//!
//! This crate holds the small enums every layer names identically
//! (`PermissionMode`, `ApprovalKind`) so config, sandbox, actor, and
//! frontends cannot drift apart. It deliberately contains no request or
//! event types: the live `Submission { id, op }` / `Event { id, msg }`
//! surface lives in `operations-wire`, the single source of truth for
//! frontend communication.

use serde::{Deserialize, Serialize};

/// Permission mode (SPEC section 12; wire strings match config's `permission_mode`).
///
/// Lives in protocol rather than sandbox: it is the wire type of
/// `SetPermissionMode`, same as `StopReason`; sandbox only holds the
/// decision logic and depends on this type.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum PermissionMode {
    /// Write / exec / destructive tools need per-call approval (hits on allow rules skip approval).
    #[serde(rename = "default")]
    Default,
    /// Only read-only tools are usable; non-read-only tools are denied straight back into the model.
    #[serde(rename = "plan")]
    Plan,
    /// File edits (write_file / edit_file) auto-approve; shell and friends still need approval.
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    /// Approve everything (deny rules still apply).
    // TODO(post-P2): entering this mode requires a confirmation phrase (SPEC section 12); frontend interaction waits on the TUI.
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
}

impl PermissionMode {
    /// Parses a mode string from config / the frontend (same names as the serde wire form).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "plan" => Some(Self::Plan),
            "acceptEdits" => Some(Self::AcceptEdits),
            "bypassPermissions" => Some(Self::BypassPermissions),
            _ => None,
        }
    }

    /// Wire string (shared by config and display).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
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
        // Lock the PermissionMode wire form (matches the config `permission_mode` strings;
        // note acceptEdits / bypassPermissions are camelCase, not snake_case).
        let mode_cases: [(PermissionMode, &str); 4] = [
            (PermissionMode::Default, "default"),
            (PermissionMode::Plan, "plan"),
            (PermissionMode::AcceptEdits, "acceptEdits"),
            (PermissionMode::BypassPermissions, "bypassPermissions"),
        ];
        for (mode, tag) in mode_cases {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!(r#""{tag}""#), "PermissionMode tag drifted");
            assert_eq!(PermissionMode::parse(tag), Some(mode));
            assert_eq!(mode.to_string(), tag);
        }
        assert_eq!(PermissionMode::parse("Default"), None);
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
