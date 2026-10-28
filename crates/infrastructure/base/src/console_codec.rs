/*!
 * @file ConsoleCodec
 * @description Console code-page decoding for child process output.
 *
 * Responsibilities:
 * - Decode captured child output the way the local console would:
 *   valid UTF-8 byte-for-byte, otherwise per-line fallback to the
 *   system ANSI code page.
 * - Serve every path that shows raw child bytes (the shell tool's
 *   captured output, the console `!` shell pump, the external status
 *   line) so one codec decision covers them all.
 *
 * This module must not depend on: process spawning or the runtime.
 */

//! Console code-page decoding.
//!
//! Children inherit the console code page rather than always speaking
//! UTF-8: on a zh-CN Windows host the `cmd` builtins (`dir`, `type`,
//! `echo`) emit GBK, so a plain `String::from_utf8_lossy` turns every
//! non-ASCII path or message into U+FFFD diamond noise. Entirely valid
//! UTF-8 still wins byte-for-byte (git, node, and most dev tools emit
//! UTF-8 regardless of locale); otherwise each invalid line falls back
//! to the system ANSI code page, so mixed pipelines keep the parts
//! that were UTF-8. A GBK line that happens to be valid UTF-8 stays
//! undetected — the ambiguity is unresolvable from bytes alone, and
//! showing Latin misreads beats showing replacement chars.

/// Decode one captured child stream the way a local console would.
pub fn decode_console_output(bytes: &[u8]) -> String {
    decode_with_fallback(bytes, console_codec())
}

/// Decode one line's bytes (newline handling is the caller's): valid
/// UTF-8 as-is, else the console code page, else lossy.
pub fn decode_console_line(line: &[u8]) -> String {
    match std::str::from_utf8(line) {
        Ok(text) => text.to_owned(),
        Err(_) => match console_codec() {
            Some(codec) => codec.decode_without_bom_handling(line).0.into_owned(),
            None => String::from_utf8_lossy(line).into_owned(),
        },
    }
}

/// The decode itself, parameterized on the fallback so tests can pin
/// the code page without depending on the host locale.
fn decode_with_fallback(bytes: &[u8], codec: Option<&'static encoding_rs::Encoding>) -> String {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_owned();
    }
    let Some(codec) = codec else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    // Split on `\n`: it cannot appear inside a multi-byte sequence in
    // UTF-8 (continuation bytes are >= 0x80) or in the CJK code pages
    // (GBK/Big5/Shift_JIS/EUC-KR trail bytes all start at 0x40).
    let mut out = String::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        match std::str::from_utf8(line) {
            Ok(text) => out.push_str(text),
            Err(_) => out.push_str(&codec.decode_without_bom_handling(line).0.into_owned()),
        }
    }
    out
}

/// The system ANSI code page decoder, resolved once. `None` off
/// Windows (host locales there are UTF-8) and for code pages without a
/// WHATWG mapping (the lossy decode stays the answer).
#[cfg(windows)]
fn console_codec() -> Option<&'static encoding_rs::Encoding> {
    use std::sync::OnceLock;
    static CODEC: OnceLock<Option<&'static encoding_rs::Encoding>> = OnceLock::new();
    *CODEC.get_or_init(|| {
        // SAFETY: `GetACP` takes no arguments and has no preconditions.
        // nosemgrep: rust.lang.security.unsafe-usage.unsafe-usage
        let code_page = unsafe { windows_sys::Win32::Globalization::GetACP() };
        Some(match code_page {
            // UTF-8 consoles never reach the fallback.
            65001 => return None,
            932 => encoding_rs::SHIFT_JIS,
            936 => encoding_rs::GBK,
            949 => encoding_rs::EUC_KR,
            950 => encoding_rs::BIG5,
            874 => encoding_rs::WINDOWS_874,
            1250 => encoding_rs::WINDOWS_1250,
            1251 => encoding_rs::WINDOWS_1251,
            1252 => encoding_rs::WINDOWS_1252,
            1253 => encoding_rs::WINDOWS_1253,
            1254 => encoding_rs::WINDOWS_1254,
            1255 => encoding_rs::WINDOWS_1255,
            1256 => encoding_rs::WINDOWS_1256,
            1257 => encoding_rs::WINDOWS_1257,
            1258 => encoding_rs::WINDOWS_1258,
            // Unmapped code page: the Western single-byte page is the
            // least-destructive guess.
            _ => encoding_rs::WINDOWS_1252,
        })
    })
}

/// Off Windows the lossy decode is the behavior.
#[cfg(not(windows))]
fn console_codec() -> Option<&'static encoding_rs::Encoding> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Valid UTF-8 decodes byte-for-byte regardless of the host codec.
    #[test]
    fn valid_utf8_wins_byte_for_byte() {
        let bytes = "café 你好".as_bytes();
        assert_eq!(decode_console_output(bytes), "café 你好");
        assert_eq!(decode_console_line(bytes), "café 你好");
    }

    /// GBK bytes decode through the fallback instead of turning into
    /// U+FFFD noise. "的目录" starts with 0xB5 (a UTF-8 continuation
    /// byte), so the line cannot be mistaken for valid UTF-8 — unlike
    /// shorter GBK strings that happen to re-read as Latin UTF-8.
    /// (Pinned to GBK so the assertion holds on every host locale.)
    #[test]
    fn gbk_lines_decode_through_the_fallback() {
        let (gbk, _, _) = encoding_rs::GBK.encode("2026/09/28  23:13    <DIR>          的目录\n");
        let out = decode_with_fallback(&gbk, Some(encoding_rs::GBK));
        assert!(out.contains("的目录"), "{out:?}");
        assert!(!out.contains('\u{FFFD}'), "{out:?}");
    }

    /// Mixed pipelines keep the UTF-8 parts byte-for-byte: only lines
    /// that fail UTF-8 go through the code page.
    #[test]
    fn mixed_utf8_and_gbk_lines_each_decode_correctly() {
        let (gbk_line, _, _) = encoding_rs::GBK.encode("新建文件夹\n");
        let mut mixed = "git log output: 历史\n".as_bytes().to_vec();
        mixed.extend_from_slice(&gbk_line);
        let out = decode_with_fallback(&mixed, Some(encoding_rs::GBK));
        assert!(out.contains("git log output: 历史\n"), "{out:?}");
        assert!(out.contains("新建文件夹"), "{out:?}");
        assert!(!out.contains('\u{FFFD}'), "{out:?}");
    }

    /// Bytes no code page maps cleanly degrade; the ASCII around them
    /// survives. 0xFF is outside GBK's lead range (0x81-0xFE), so the
    /// decoder cannot pair it away with the byte that follows.
    #[test]
    fn undecodable_bytes_keep_their_ascii() {
        let out = decode_with_fallback(&[0x61, 0xff, 0x62, 0x0a], Some(encoding_rs::GBK));
        assert!(out.contains('a'), "{out:?}");
        assert!(out.contains('b'), "{out:?}");
        assert!(out.ends_with('\n'), "{out:?}");
    }

    /// The single-line entry point mirrors the whole-buffer behavior on
    /// pinned GBK input.
    #[test]
    fn line_decode_matches_the_buffer_decode() {
        let (gbk, _, _) = encoding_rs::GBK.encode("的目录");
        assert_eq!(
            decode_console_line(&gbk),
            decode_with_fallback(&gbk, Some(encoding_rs::GBK))
        );
    }

    /// Entirely valid UTF-8 wins regardless of the fallback: the
    /// code page never rewrites it.
    #[test]
    fn valid_utf8_survives_the_fallback() {
        let text = "完全合法的 UTF-8 输出 ✔\nsecond line\n";
        let out = decode_with_fallback(text.as_bytes(), Some(encoding_rs::GBK));
        assert_eq!(out, text);
    }

    /// Without a fallback (non-Windows, or code page 65001) the decode
    /// is the previous lossy behavior — for bytes that fail UTF-8.
    #[test]
    fn no_fallback_matches_the_lossy_decode() {
        let (gbk, _, _) = encoding_rs::GBK.encode("的目录\n");
        let out = decode_with_fallback(&gbk, None);
        assert!(out.contains('\u{FFFD}'), "{out:?}");
        assert_eq!(out, String::from_utf8_lossy(&gbk));
    }
}
