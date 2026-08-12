//! 路径防逃逸：把模型给出的路径解析到 `ToolCtx::cwd` 之下。
//!
//! 安全校验一律在 canonicalize 后的真实路径上进行（Windows 上 canonicalize
//! 带 `\\?\` 前缀，比较两侧必须同态才可比）；返回的则是以（未 canonicalize
//! 的）cwd 锚定的词法规范化路径，便于调用方展示与比较。
//!
//! TOCTOU 假设：校验与实际使用（read/write）之间，路径上的 symlink 可能被
//! 替换，本模块无法防护该竞态；M1 威胁模型接受这一窗口，后续里程碑再考虑
//! fd 锚定（openat 语义）等强化手段。

use std::path::{Component, Path, PathBuf};

use crate::{Result, ToolsError};

/// 把用户给出的 path 解析到 ctx.cwd 之下；逃逸（.. 越界、绝对路径到他盘/他目录）返回 PathEscape
pub(crate) fn resolve(ctx: &crate::ToolCtx, path: &str) -> Result<PathBuf> {
    if path.is_empty() {
        return Err(ToolsError::InvalidInput {
            message: "path 不能为空".to_owned(),
        });
    }
    // join 时绝对路径整体替换 cwd，相对路径拼到 cwd 之下；先做词法规范化消除 `.` / `..`。
    let joined = ctx.cwd.join(path);
    let normalized = normalize_lexically(&joined);
    let cwd_canon = ctx.cwd.canonicalize().map_err(ToolsError::Io)?;

    // 用 symlink_metadata 判断存在性：symlink 自身算“存在”，断链 symlink 会落在
    // 已存在分支并在 canonicalize 处报错，避免顺着断链把文件写到 cwd 之外。
    match std::fs::symlink_metadata(&normalized) {
        Ok(_) => {
            // 已存在：解开 symlink 后的真实路径必须在 cwd 真实路径之下。
            let canon = normalized.canonicalize().map_err(ToolsError::Io)?;
            if canon.starts_with(&cwd_canon) {
                Ok(normalized)
            } else {
                Err(escape(path))
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 不存在（write_file 场景）：锚定最近的已存在祖先，canonicalize 后拼接剩余
            // 部分再做前缀校验；通过则返回词法规范化路径（以未 canonicalize 的 cwd 开头）。
            let anchor = normalized
                .ancestors()
                .find(|a| a.symlink_metadata().is_ok())
                .ok_or_else(|| escape(path))?;
            let rest = normalized
                .strip_prefix(anchor)
                .expect("anchor 必为 normalized 的前缀");
            let anchor_canon = anchor.canonicalize().map_err(ToolsError::Io)?;
            let candidate = normalize_lexically(&anchor_canon.join(rest));
            if candidate.starts_with(&cwd_canon) {
                Ok(normalized)
            } else {
                Err(escape(path))
            }
        }
        Err(e) => Err(ToolsError::Io(e)),
    }
}

/// 构造 PathEscape 错误，记录用户原始输入便于排查。
fn escape(path: &str) -> ToolsError {
    ToolsError::PathEscape {
        path: path.to_owned(),
    }
}

/// 纯词法规范化：消除 `.`；`..` 能弹出上一级普通目录则弹出，
/// 已到根无法弹出的保留原样——后续 starts_with 校验会拒绝这类结果。
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(seg) => out.push(seg),
            Component::ParentDir => {
                let can_pop = matches!(out.components().next_back(), Some(Component::Normal(_)));
                if can_pop {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            // Prefix / RootDir 原样保留
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        assert!(
            super::resolve(&ctx, "inside/ok.txt")
                .unwrap()
                .ends_with("ok.txt")
        );
        assert!(super::resolve(&ctx, "../escape.txt").is_err());
        assert!(super::resolve(&ctx, "../../escape.txt").is_err());
        // 绝对路径到他盘/他目录：选各平台上几乎必然存在、且必不在 tempdir 下的路径
        #[cfg(windows)]
        assert!(super::resolve(&ctx, "C:/Windows/evil.txt").is_err());
        #[cfg(unix)]
        assert!(super::resolve(&ctx, "/etc/passwd").is_err());
    }

    #[test]
    fn resolves_nonexistent_file_under_cwd() {
        // write_file 场景：目标不存在，但锚定已存在祖先后仍在 cwd 内
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        let p = super::resolve(&ctx, "newdir/newfile.txt").unwrap();
        assert!(p.starts_with(dir.path()));
    }

    /// symlink/junction 指向 cwd 之外：read_file / write_file 均须拒绝且外部零污染。
    /// 创建失败（权限或平台策略）时打印提示并跳过，不作为失败。
    #[tokio::test]
    async fn symlink_escape_is_rejected() {
        use crate::Tool;

        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ToolCtx {
            cwd: dir.path().to_path_buf(),
            deny_env: Vec::new(),
        };
        let link = dir.path().join("link");

        // Windows 用 junction（mklink /J 为 cmd 内建命令，tempdir 内无需提权）
        #[cfg(windows)]
        let made = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(outside.path(), &link).is_ok();
        #[cfg(not(any(windows, unix)))]
        let made = false;
        if !made || link.symlink_metadata().is_err() {
            eprintln!("无法创建 symlink/junction（权限或平台策略），跳过 symlink 逃逸测试");
            return;
        }

        let w = crate::fs_tools::WriteFile
            .execute(
                serde_json::json!({"path":"link/evil.txt","content":"x"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(w.is_error);
        let r = crate::fs_tools::ReadFile
            .execute(serde_json::json!({"path":"link/evil.txt"}), &ctx)
            .await
            .unwrap();
        assert!(r.is_error);
        // 外部目录零污染
        assert!(!outside.path().join("evil.txt").exists());
    }

    /// 兄弟目录前缀混淆：纯字符串 starts_with 会把 abd 误判在 abc 之内，
    /// component 级比较必须拒绝。
    #[test]
    fn rejects_sibling_prefix_confusion() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("abc");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::create_dir(root.path().join("abd")).unwrap();
        let ctx = crate::ToolCtx {
            cwd,
            deny_env: Vec::new(),
        };
        assert!(super::resolve(&ctx, "../abd/x.txt").is_err());
    }
}
