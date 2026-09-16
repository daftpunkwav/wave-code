/*! @file ReadImage
 * @description Image-file attachment tool with magic sniffing and size caps.
 *
 * Responsibilities:
 * - Resolve image paths under cwd via path_guard
 * - Sniff magic bytes (never trust extensions) and enforce the 5MB cap
 * - Return compact mime/base64 JSON for mapping onto ContentBlock::Image
 *
 * This module must not depend on: UI-layer components, network access.
 */

//! `view` tool (read-only): load an image file as a model attachment.
//!
//! The file is resolved under `cwd` via `path_guard`, sniffed by magic bytes
//! (extension is never trusted), capped at 5 MB decoded (no downscaling;
//! oversized images are rejected with the cap named), and returned as compact
//! JSON `{"mime": ..., "base64": ...}` for the caller to map onto
//! `ContentBlock::Image`. Non-image magic is a business error.

use serde_json::{Value, json};

use super::{req_str, resolve_path};
use crate::{Result, Tool, ToolCtx, ToolOutput};

/// Maximum image file bytes (mirrors the llm attachment cap; no downscaling).
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Sniff the image MIME from magic bytes; `None` means not a supported image.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && &bytes[..8] == b"\x89PNG\r\n\x1a\n" {
        Some("image/png")
    } else if bytes.len() >= 3 && &bytes[..3] == b"\xFF\xD8\xFF" {
        Some("image/jpeg")
    } else if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Read an image file as an attachment (read-only).
pub struct ReadImage;

#[async_trait::async_trait]
impl Tool for ReadImage {
    fn name(&self) -> &str {
        "view"
    }

    fn description(&self) -> &str {
        "Read an image file (png, jpeg, webp, gif; max 5MB) inside the working \
         directory as a model attachment. Returns JSON with mime and base64 \
         fields; map it onto an image content block. Larger files are rejected \
         (no downscaling); non-image files are rejected."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path of the image file, relative to the working directory"
                }
            },
            "required": ["path"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput> {
        let path = match req_str(&input, "path") {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        let path = match resolve_path(ctx, path)? {
            Ok(p) => p,
            Err(out) => return Ok(out),
        };
        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput {
                    content: format!("file not found: {}", path.display()),
                    is_error: true,
                });
            }
            Err(e) => return Err(e.into()),
        };
        if meta.is_dir() {
            return Ok(ToolOutput {
                content: format!("path is a directory, not an image: {}", path.display()),
                is_error: true,
            });
        }
        if meta.len() > MAX_IMAGE_BYTES {
            return Ok(ToolOutput {
                content: format!(
                    "image too large ({} bytes), max {} bytes (5MB cap, no downscaling)",
                    meta.len(),
                    MAX_IMAGE_BYTES
                ),
                is_error: true,
            });
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ToolOutput {
                    content: format!("file not found: {}", path.display()),
                    is_error: true,
                });
            }
            Err(e) => return Err(e.into()),
        };
        let mime = match sniff_mime(&bytes) {
            Some(m) => m,
            None => {
                return Ok(ToolOutput {
                    content: format!(
                        "not a supported image (png, jpeg, webp, gif magic required): {}",
                        path.display()
                    ),
                    is_error: true,
                });
            }
        };
        use base64::Engine as _;
        let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(ToolOutput {
            content: serde_json::json!({"mime": mime, "base64": base64_data}).to_string(),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_covers_allowlist_and_rejects_text() {
        assert_eq!(sniff_mime(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff_mime(b"\xFF\xD8\xFFrest"), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89arest"), Some("image/gif"));
        assert_eq!(
            sniff_mime(b"RIFF\x00\x00\x00\x00WEBPrest"),
            Some("image/webp")
        );
        assert_eq!(sniff_mime(b"hello text"), None);
        assert_eq!(sniff_mime(b""), None);
    }

    #[tokio::test]
    async fn view_roundtrip_and_rejections() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        // Minimal PNG magic + payload.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0u8; 16]);
        std::fs::write(dir.path().join("a.png"), &png).unwrap();
        let out = ReadImage
            .execute(serde_json::json!({"path": "a.png"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        let v: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["mime"], "image/png");
        assert!(!v["base64"].as_str().unwrap().is_empty());
        // Text file rejected by magic sniff.
        std::fs::write(dir.path().join("b.txt"), "hello").unwrap();
        let out = ReadImage
            .execute(serde_json::json!({"path": "b.txt"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
        // Escape rejected.
        let out = ReadImage
            .execute(serde_json::json!({"path": "../evil.png"}), &ctx)
            .await
            .unwrap();
        assert!(out.is_error);
    }
}
