//! skills 的 core 侧编排（P7，SPEC §8）。
//!
//! skills crate 无 workspace 内依赖（SPEC §3 依赖矩阵），凡涉及 tools /
//! subagent 的能力都落在 core（core→skills 边由矩阵允许）：
//!
//! - [`SkillSessionConfig`]：装配层（cli bootstrap）注入
//!   [`crate::session::SessionConfig`] 的技能面（发现产物 `Arc<SkillSet>`）；
//! - [`SkillTool`]：`skill` 工具——模型触发入口（SPEC §11.2）；
//! - 清单注入：[`catalog_budget_chars`] 换算 1% 窗口预算，Session 启动时
//!   渲染一次注入 prompt 分层 builder 的 skills 槽位（SPEC §5.4 顺序）；
//! - slash 直调（`/name [args]`）：`Session::invoke_skill`（session.rs），
//!   与本模块共享展开 / fork 逻辑。
//!
//! **执行模式**：
//! - inline：正文展开（`$ARGUMENTS` / `${WAVECODE_SKILL_DIR}`）后作为
//!   ToolResult 回灌——tool_result 本就随下一请求的 user 消息进历史，
//!   与 SPEC"展开为 user 消息"在模型视角等效（择一注释：不额外再 push
//!   一条重复正文的 user 消息，避免历史膨胀）；
//! - fork：以 skill 正文为指令派生后台子代理（SubagentManager，
//!   `allowed-tools` 按 registry 过滤子代理工具面——构造级限定）。
//!
//! **allowed-tools 首版语义（诚实声明）**：fork 为构造级过滤（子代理
//! registry 按名子集，限定整个子代理生命周期）；inline 为 **turn 级**
//! 白名单（激活写入 registry 共享句柄，执行管道在 PreToolUse / 审批前
//! 拦截名单外工具，turn 入口清零）——"激活期间"取当前 turn 为界，
//! 跨 turn 的持续限定留待后续（需要激活/去激活的事件边界设计）。

use std::sync::Arc;

use serde_json::{Value, json};
use wavecode_tools::{Tool, ToolAllowlist, ToolCtx, ToolOutput};

use crate::subagent::{SubagentManager, SubagentType, TaskSpec};

// skills crate 的公开面经 core 再导出：cli 装配层（bootstrap 发现与注入）
// 不新增 cli→skills 依赖边（SPEC §3 矩阵 cli 行无 skills；core 行本已允许）。
pub use wavecode_skills::{
    Discovery, Skill, SkillContext, SkillError, SkillMeta, SkillRoot, SkillSet, SkillSource,
    discover, standard_roots,
};

/// 装配层注入的技能面（`SessionConfig.skills`；None = 无 skills 能力——
/// 子代理自身的 Session 即此形态，隔离上下文不挂 skill 触发面）。
#[derive(Debug, Clone)]
pub struct SkillSessionConfig {
    /// 覆盖消解后的技能集（发现产物）。
    pub set: Arc<SkillSet>,
}

/// 清单注入预算（SPEC §8.2"预算 = 上下文窗口 1%"）：窗口 token 的 1%
/// 换算为字符额度（复用上下文管线的估算比率，近似即可——预算只为防
/// 清单无界膨胀）。
pub fn catalog_budget_chars(context_window: u64, chars_per_token: usize) -> usize {
    ((context_window / 100) as usize).saturating_mul(chars_per_token)
}

/// 触发产物：inline 展开文本 / fork 派生规格（`Session::invoke_skill`
/// 与 [`SkillTool`] 共用，见模块注释）。
pub(crate) enum SkillInvocation {
    /// inline：展开后的正文（回灌 / 作为 turn 输入）。
    Inline(String),
    /// fork：子代理派生规格。
    Fork(TaskSpec),
}

/// 构造一次 skill 触发（查找 + user_invocable 校验 + 展开 / fork 规格）。
/// 失败返回业务错误文本（调用方回灌模型 / 发 Error 事件）。
pub(crate) fn plan_invocation(
    set: &SkillSet,
    name: &str,
    args: &str,
    via_slash: bool,
) -> Result<SkillInvocation, String> {
    let Some(skill) = set.get(name) else {
        let available: Vec<&str> = set.iter().map(|s| s.name.as_str()).collect();
        return Err(format!(
            "unknown skill: {name} (available: {})",
            if available.is_empty() {
                "(none)".to_owned()
            } else {
                available.join(", ")
            }
        ));
    };
    // user-invocable 只约束 `/name` 直调（SPEC §8.1）；模型经 skill 工具
    // 触发不受此限（清单注入本就只面向模型自动触发）。
    if via_slash && !skill.meta.user_invocable {
        return Err(format!("skill `{name}` is not user-invocable"));
    }
    let expanded = skill.expand(args);
    match skill.meta.context {
        SkillContext::Inline => Ok(SkillInvocation::Inline(expanded)),
        SkillContext::Fork => {
            // fork（SPEC §8.2"以 skill 正文为系统提示派生 subagent"）：
            // 诚实近似——子代理尚无自定义系统提示词注入点（P5 注释在案），
            // 首版以 preamble 机制把 skill 正文拼在子代理输入前部（与内置
            // 类型的前言同路径）；args 作为任务输入。
            let spec = TaskSpec {
                description: format!("skill: {name}"),
                prompt: format!(
                    "Follow the skill instructions above to complete the request.\n\nArguments: {}",
                    if args.trim().is_empty() {
                        "(none)"
                    } else {
                        args.trim()
                    }
                ),
                subagent_type: SubagentType::GeneralPurpose,
                preamble: Some(expanded),
                allowed_tools: if skill.meta.allowed_tools.is_empty() {
                    None
                } else {
                    Some(skill.meta.allowed_tools.clone())
                },
            };
            Ok(SkillInvocation::Fork(spec))
        }
    }
}

/// `skill` 工具（SPEC §11.2）：模型触发 skill 的入口。
///
/// inline 的 `allowed-tools` 激活为 turn 级白名单（见模块注释）；fork 经
/// [`SubagentManager`] 派生后台子代理（管理器缺失 = 无子代理能力的会话
/// ——子代理自身 Session，报业务错误回灌）。
pub struct SkillTool {
    skills: Arc<SkillSet>,
    allowlist: ToolAllowlist,
    manager: Option<Arc<SubagentManager>>,
}

impl SkillTool {
    /// 装配构造（`Session::new` / `Session::with_subagents` 注册）。
    pub fn new(
        skills: Arc<SkillSet>,
        allowlist: ToolAllowlist,
        manager: Option<Arc<SubagentManager>>,
    ) -> Self {
        Self {
            skills,
            allowlist,
            manager,
        }
    }

    /// inline 激活的副作用：写入 turn 级工具面白名单（空名单 = 不限）。
    fn activate_allowed_tools(&self, skill: &Skill) {
        if skill.meta.allowed_tools.is_empty() {
            return;
        }
        self.allowlist
            .set(Some(skill.meta.allowed_tools.iter().cloned().collect()));
    }
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Invoke a skill by name. Inline skills expand their instructions into the conversation; \
         fork skills spawn a background subagent guided by the skill body. Available skills are \
         listed in the system prompt's skills reminder."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name (from the skills list in the system prompt)"
                },
                "args": {
                    "type": "string",
                    "description": "Arguments passed to the skill ($ARGUMENTS expansion)"
                }
            },
            "required": ["name"]
        })
    }

    fn is_read_only(&self) -> bool {
        // inline 会写白名单句柄、fork 派生子代理：非只读 → 串行段执行
        //（与 task 工具同例；审批经 sandbox 非只读默认策略挂接）。
        false
    }

    async fn execute(&self, input: Value, _ctx: &ToolCtx) -> wavecode_tools::Result<ToolOutput> {
        let err = |reason: String| {
            Ok(ToolOutput {
                content: reason,
                is_error: true,
            })
        };
        let name = match input.get("name").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s.trim(),
            _ => {
                return err(
                    "missing or invalid parameter 'name' (non-empty string required)".to_owned(),
                );
            }
        };
        let args = input.get("args").and_then(Value::as_str).unwrap_or("");
        match plan_invocation(&self.skills, name, args, false) {
            Err(reason) => err(reason),
            Ok(SkillInvocation::Inline(expanded)) => {
                let skill = self.skills.get(name).expect("plan_invocation 已确认存在");
                self.activate_allowed_tools(skill);
                // 展开正文作为 ToolResult 回灌（模块注释：与"展开为 user
                // 消息"模型视角等效）；白名单限定在结果之后的工具调用生效。
                Ok(ToolOutput {
                    content: expanded,
                    is_error: false,
                })
            }
            Ok(SkillInvocation::Fork(spec)) => {
                let Some(manager) = &self.manager else {
                    return err(format!(
                        "skill `{name}` requires subagent capability (context: fork), which is \
                         not available in this session"
                    ));
                };
                let task_id = manager.spawn_background(spec);
                Ok(ToolOutput {
                    content: format!(
                        "Skill `{name}` spawned as background subagent {task_id}; its completion \
                         will be reported back as a task-notification. Use task_output to poll \
                         its status."
                    ),
                    is_error: false,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skill(name: &str, context: SkillContext, allowed: &[&str], invocable: bool) -> Skill {
        Skill {
            name: name.to_owned(),
            dir: PathBuf::from("/tmp/sk"),
            source: SkillSource::Project,
            meta: SkillMeta {
                description: format!("{name} 描述"),
                when_to_use: None,
                allowed_tools: allowed.iter().map(|s| s.to_string()).collect(),
                context,
                user_invocable: invocable,
                argument_hint: None,
                paths: vec![],
            },
            body: format!("处理 $ARGUMENTS（目录 ${{{}}}", "WAVECODE_SKILL_DIR"),
        }
    }

    fn set_of(skills: Vec<Skill>) -> SkillSet {
        let mut set = SkillSet::default();
        for s in skills {
            set.add(s);
        }
        set
    }

    /// plan_invocation：inline 展开（$ARGUMENTS 替换）；slash 校验
    /// user-invocable；模型触发不受限。
    #[test]
    fn plan_inline_expands_arguments() {
        let set = set_of(vec![skill("fix", SkillContext::Inline, &[], true)]);
        let Ok(SkillInvocation::Inline(text)) = plan_invocation(&set, "fix", "崩溃", false)
        else {
            panic!("inline 应展开")
        };
        assert!(text.contains("处理 崩溃"), "{text}");
        // slash 且 user_invocable=false → 拒绝；模型触发放行。
        let set = set_of(vec![skill("hidden", SkillContext::Inline, &[], false)]);
        assert!(plan_invocation(&set, "hidden", "", true).is_err());
        assert!(plan_invocation(&set, "hidden", "", false).is_ok());
        // 未知名：错误文本含可用清单。
        let err = match plan_invocation(&set, "nope", "", false) {
            Ok(_) => panic!("未知名应报错"),
            Err(e) => e,
        };
        assert!(err.contains("unknown skill: nope") && err.contains("hidden"));
    }

    /// plan_invocation：fork 规格——正文为 preamble，args 为 prompt，
    /// allowed-tools 进入派生规格。
    #[test]
    fn plan_fork_builds_task_spec() {
        let set = set_of(vec![skill(
            "review",
            SkillContext::Fork,
            &["read_file", "grep"],
            true,
        )]);
        let Ok(SkillInvocation::Fork(spec)) = plan_invocation(&set, "review", "src/", false) else {
            panic!("fork 应产出 TaskSpec")
        };
        assert!(spec.preamble.as_deref().unwrap().contains("处理 src/"));
        assert!(spec.prompt.contains("Arguments: src/"));
        assert_eq!(
            spec.allowed_tools,
            Some(vec!["read_file".to_owned(), "grep".to_owned()])
        );
        assert_eq!(spec.description, "skill: review");
    }
}
