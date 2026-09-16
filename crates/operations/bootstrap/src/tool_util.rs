/*!
 * @file ToolUtil
 * @description Small helpers shared by the model-invoked tool modules in
 * this crate (promote a helper here once a second tool module needs it;
 * no policy inside).
 *
 * This module must not depend on: drivers, actors, sessions, or models.
 */

//! Shared tool-module helpers.

use wavecode_tools::ToolOutput;

/// Required string field helper: missing or blank becomes a business
/// error the model can self-correct from.
pub(crate) fn required_str<'a>(
    input: &'a serde_json::Value,
    field: &str,
) -> std::result::Result<&'a str, ToolOutput> {
    match input.get(field).and_then(|value| value.as_str()) {
        Some(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ToolOutput {
            content: format!("missing required field: {field}"),
            is_error: true,
        }),
    }
}
