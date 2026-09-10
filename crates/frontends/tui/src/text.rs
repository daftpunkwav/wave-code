//! 终端文本工具：控制字符净化与按字符截断（双前端单一事实源）。
//!
//! cli 经 SPEC §3 矩阵允许的 cli→tui 单向边复用本模块（2026-09 收口；
//! 此前为两份手工同步的镜像复制，已并入此处删除 cli 侧副本）。模型 /
//! 工具来源文本必须过 [`sanitize_terminal`] 才能进终端：防 ANSI / OSC
//! 注入擦除痕迹。

use std::borrow::Cow;

/// 是否为需剥离的控制字符：C0（保留 `\n` / `\t`）、DEL、C1（U+0080–U+009F）。
fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// 终端输出净化：剥离 C0/C1 控制字符与 ESC 序列（保留 `\n`、`\t`）。
/// 无控制字符时零拷贝返回借用。
pub fn sanitize_terminal(s: &str) -> Cow<'_, str> {
    // 快路径：无需剥离的字符直接借用。
    if !s.chars().any(is_control) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            // ESC 序列整体跳过：CSI（ESC [ … 终字节 0x40–0x7E）、
            // OSC（ESC ] … 终止于 BEL 或 ESC \）、其余按 ESC+单字符。
            match it.next() {
                Some('[') => {
                    for c in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for c in it.by_ref() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\x1b' {
                            // 假定 ST（ESC \）：多吞一字符。
                            it.next();
                            break;
                        }
                    }
                }
                // ESC+单字符序列（含孤立 ESC \）：跳过的字符已消费。
                _ => {}
            }
            continue;
        }
        if !is_control(c) {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// 按字符数截断（非字节，防切断 UTF-8），超长时末位替换为省略号 `…`。
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 净化语义锁定用例（cli 侧复用同一实现，无需重复用例）。
    #[test]
    fn sanitize_strips_control_sequences() {
        assert_eq!(sanitize_terminal("a\x1b[2Jb"), "ab");
        assert_eq!(sanitize_terminal("\x1b[1;31m红\x1b[0m"), "红");
        assert_eq!(sanitize_terminal("x\x1b]52;;cGF5bG9hZA==\x07y"), "xy");
        assert_eq!(sanitize_terminal("x\x1b]0;title\x1b\\y"), "xy");
        assert_eq!(sanitize_terminal("p\x07q\x08r"), "pqr");
        assert_eq!(sanitize_terminal("a\u{9b}1;31mb"), "a1;31mb");
        let s = "正常中文🦀\n换行\t制表符";
        let sanitized = sanitize_terminal(s);
        assert_eq!(sanitized, s);
        assert!(matches!(sanitized, Cow::Borrowed(_)), "应零拷贝借用");
    }

    #[test]
    fn truncate_multibyte_utf8_by_chars() {
        let t = truncate_chars(&"汉".repeat(200), 80);
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('…'));
        assert_eq!(truncate_chars("短", 80), "短");
    }
}
