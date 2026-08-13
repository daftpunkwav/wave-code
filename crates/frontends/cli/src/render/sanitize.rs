//! 终端净化（阶段 4 拆分自 render.rs）：剥离控制序列，防注入逃逸。

use std::borrow::Cow;

fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// 终端输出净化：剥离 C0/C1 控制字符与 ESC 序列（保留 `\n`、`\t`），
/// 防模型 / 工具来源文本携带 ANSI / OSC 序列（清屏 `\x1b[2J`、OSC 52 写
/// 剪贴板、BEL 等）擦除工具调用痕迹——M1 无审批，该摘要是用户唯一的
/// 实时线索。无控制字符时零拷贝返回借用。
pub(crate) fn sanitize_terminal(s: &str) -> Cow<'_, str> {
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
                            // 假定 ST（ESC \）：多吞一字符（截断的 OSC 同样
                            // 保守剥离，ESC 绝不进终端）。
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
