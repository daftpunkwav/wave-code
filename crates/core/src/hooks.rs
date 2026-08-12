//! hooks 的 core 侧编排（P7，SPEC §9）。
//!
//! hooks crate 无 workspace 内依赖（SPEC §3 依赖矩阵），config 层只做
//! `[hooks]` 原始解析（config 同样无 workspace 内依赖）——本模块负责把
//! config 原始表转换为 hooks crate 的 [`HookEngine`]，事件点的挂接位置：
//! - PreToolUse / PostToolUse：session.rs 工具执行管道（SPEC §11.1 顺序：
//!   查找 → PreToolUse → 审批 → execute → PostToolUse）；
//! - UserPromptSubmit / Stop：session.rs turn 入口与终态；
//! - PreCompact / PostCompact：session.rs 压缩管线；
//! - SessionStart / SessionEnd：cli bootstrap / 退出路径（core 再导出
//!   引擎类型，cli 不新增 cli→hooks 依赖边）。

use std::collections::HashMap;

use wavecode_config::HookRuleSet;

// hooks crate 的公开面经 core 再导出（cli 装配层使用，同 memory/skills 先例）。
pub use wavecode_hooks::{
    DEFAULT_TIMEOUT_MS, HookDef, HookEngine, HookEventPoint, HookInput, HookReport, HookVerdict,
};

/// hooks 配置转换错误。
#[derive(Debug, thiserror::Error)]
pub enum HooksConfigError {
    /// 未知事件点名（`[hooks.<EventPoint>]` 表名非法）。
    #[error(
        "未知 hook 事件点: {0}（合法值：PreToolUse / PostToolUse / UserPromptSubmit / SessionStart / SessionEnd / Stop / PreCompact / PostCompact）"
    )]
    UnknownEventPoint(String),
}

/// config 原始 hooks 表 → [`HookEngine`]。
///
/// 未知事件点名为 Err（显式失败，对齐 sandbox 规则解析的启动期显式化
/// 纪律，不静默跳过——由装配层决定降级策略）；timeout_ms / once 缺省
/// 补 hooks crate 默认值。
pub fn engine_from_config(
    raw: &HashMap<String, HookRuleSet>,
) -> Result<HookEngine, HooksConfigError> {
    let mut defs: HashMap<HookEventPoint, Vec<HookDef>> = HashMap::new();
    for (name, rule_set) in raw {
        let point = HookEventPoint::parse(name)
            .ok_or_else(|| HooksConfigError::UnknownEventPoint(name.clone()))?;
        for rule in rule_set.rules() {
            defs.entry(point).or_default().push(HookDef {
                matcher: rule.matcher.clone(),
                command: rule.command.clone(),
                timeout_ms: rule.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS),
                once: rule.once.unwrap_or(false),
            });
        }
    }
    Ok(HookEngine::new(defs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wavecode_config::HookRule;

    fn raw(entries: &[(&str, Vec<HookRule>)]) -> HashMap<String, HookRuleSet> {
        entries
            .iter()
            .map(|(name, rules)| {
                (
                    name.to_string(),
                    HookRuleSet::Many(
                        rules
                            .iter()
                            .map(|r| HookRule {
                                matcher: r.matcher.clone(),
                                command: r.command.clone(),
                                timeout_ms: r.timeout_ms,
                                once: r.once,
                            })
                            .collect(),
                    ),
                )
            })
            .collect()
    }

    fn rule(command: &str) -> HookRule {
        HookRule {
            matcher: None,
            command: command.to_owned(),
            timeout_ms: None,
            once: None,
        }
    }

    #[test]
    fn converts_with_defaults() {
        let raw = raw(&[(
            "PreToolUse",
            vec![
                HookRule {
                    matcher: Some("shell".to_owned()),
                    ..rule("check.sh")
                },
                rule("lint.sh"),
            ],
        )]);
        let engine = engine_from_config(&raw).unwrap();
        assert!(engine.has_hooks(HookEventPoint::PreToolUse));
        assert!(!engine.has_hooks(HookEventPoint::Stop));
        assert!(!engine.is_empty());
        // 空表 → 空引擎。
        let empty = engine_from_config(&HashMap::new()).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn unknown_event_point_is_error() {
        let raw = raw(&[("BeforeTool", vec![rule("x.sh")])]);
        let err = match engine_from_config(&raw) {
            Ok(_) => panic!("未知事件点应报错"),
            Err(e) => e,
        };
        assert!(matches!(err, HooksConfigError::UnknownEventPoint(_)));
        assert!(err.to_string().contains("BeforeTool"));
    }
}
