/*! @file WindowsConfinement
 *  @description Documented scope for Windows OS confinement (no backend).
 *
 *  Responsibilities:
 *  - Record why Windows ships no enforcing backend.
 *
 *  This module must not depend on: tools, transport, frontend crates.
 */

/// Why Windows stays on the refusing fallback: lowering integrity via
/// `SetTokenInformation` plus a kill-on-close job object needs the `windows`
/// crate for sound FFI, and that crate is not in the workspace. Adding a
/// winapi/`windows` dependency for a partial control is out of scope, so on
/// Windows `detect_backend` returns [`crate::os::UnavailableBackend`] and a
/// requested confinement fails closed (`SANDBOX_UNAVAILABLE`) instead of
/// running unconfined. A future backend must implement
/// [`crate::os::SandboxBackend`] in a separate module and join the probe
/// chain after seatbelt.
pub const WINDOWS_UNAVAILABLE_REASON: &str = "no enforcing Windows backend: integrity-level lowering plus a kill-on-close job object needs the `windows` crate, which is not a workspace dependency (no winapi/windows FFI added for a partial control); confinement requests fail closed";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_names_the_missing_crate_and_fail_closed() {
        assert!(WINDOWS_UNAVAILABLE_REASON.contains("`windows` crate"));
        assert!(WINDOWS_UNAVAILABLE_REASON.contains("fail closed"));
    }
}
