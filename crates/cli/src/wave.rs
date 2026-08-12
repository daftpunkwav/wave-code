//! 波形动画：正弦采样块字符 + 青→蓝→紫 RGB 渐变。
//!
//! 品牌的"波形"动效内核：启动横幅动画、等待模型指示共用 [`frame`]。
//! 纯函数、确定性，ANSI 启用/剥离由输出侧 anstream 决定。

use anstyle::{Color, Reset, RgbColor, Style};

/// 八级块字符（由低到高）
const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// 生成第 `phase` 帧、宽 `n` 格的波形（每格带 RGB ANSI，末尾 reset）。
pub fn frame(n: usize, phase: f32) -> String {
    let mut s = String::new();
    for i in 0..n {
        let h = ((i as f32 * 0.8 + phase * 0.9).sin() + 1.0) / 2.0;
        let ch = BLOCKS[(h * 7.0).round() as usize];
        let (r, g, b) = palette(i as f32 * 0.35 + phase * 0.25);
        let style = Style::new().fg_color(Some(Color::Rgb(RgbColor(r, g, b))));
        s.push_str(&format!("{}{}", style.render(), ch));
    }
    // 帧尾无条件 reset（每格都带 RGB 样式；anstyle 的 render_reset 对空样式会省略，故直接用 Reset）
    s.push_str(&Reset.to_string());
    s
}

/// 青→蓝→紫循环渐变：t 归一到 [0,3)，三段线性插值
fn palette(t: f32) -> (u8, u8, u8) {
    const STOPS: [(u8, u8, u8); 3] = [(64, 224, 208), (80, 140, 255), (186, 104, 255)];
    let tau = t.rem_euclid(3.0);
    let i = tau as usize;
    let f = tau - i as f32;
    let (a, b) = (STOPS[i], STOPS[(i + 1) % 3]);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f).round() as u8;
    (lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

/// REPL 启动横幅（定格帧）。`phase` 取启动动画末帧相位，视觉无缝衔接。
pub fn banner(model: &str, cwd: &std::path::Path, version: &str, phase: f32) -> String {
    let wave = frame(7, phase);
    let name = Style::new()
        .fg_color(Some(Color::Ansi(anstyle::AnsiColor::BrightCyan)))
        .bold();
    let dim = Style::new().fg_color(Some(Color::Ansi(anstyle::AnsiColor::BrightBlack)));
    format!(
        "{wave}  {name}WaveCode v{version}{name:#}\n         {dim}{model} · {cwd}{dim:#}\n{dim}提示：直接输入任务；/quit 或 /exit 退出{dim:#}\n",
        cwd = cwd.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_deterministic_and_phase_moves() {
        assert_eq!(frame(7, 0.0), frame(7, 0.0));
        assert_ne!(frame(7, 0.0), frame(7, 1.0), "相位应驱动波形变化");
    }

    #[test]
    fn frame_contains_block_chars_rgb_and_reset() {
        let f = frame(7, 0.0);
        // 7 个块字符（每个带 RGB ANSI 前缀）+ 末尾 reset
        assert_eq!(f.matches("\x1b[38;2;").count(), 7, "应为 7 段 RGB：{f:?}");
        assert!(
            f.contains('▁')
                || f.contains('▂')
                || f.contains('▃')
                || f.contains('▄')
                || f.contains('▅')
                || f.contains('▆')
                || f.contains('▇')
                || f.contains('█')
        );
        assert!(f.ends_with("\x1b[0m"), "末尾 reset：{f:?}");
    }

    #[test]
    fn palette_cycles_through_three_stops() {
        assert_eq!(palette(0.0), (64, 224, 208));
        assert_eq!(palette(1.0), (80, 140, 255));
        assert_eq!(palette(2.0), (186, 104, 255));
        assert_eq!(palette(3.0), palette(0.0), "调色板周期 3");
    }

    #[test]
    fn banner_contains_brand_model_cwd() {
        let b = banner(
            "MiniMax-M3",
            std::path::Path::new("/tmp/demo"),
            "0.1.0",
            4.2,
        );
        assert!(b.contains("WaveCode v0.1.0"));
        assert!(b.contains("MiniMax-M3"));
        assert!(b.contains("/tmp/demo"));
        assert!(b.contains("\x1b[38;2;"), "波形应为 RGB 彩色：{b:?}");
        assert!(b.contains("提示"));
    }
}
