use anyhow::Result;
use serde_json::{json, Value};
use std::io::Write;

use crate::agent::harness::traits::{
    truncate_text, Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput,
};

use super::args;

/// 写入文本文件（原子写：临时文件 + fsync + rename）
pub struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "write_file",
            "写入文件",
            "在工作区内创建或覆盖文本文件。覆盖已存在的文件必须显式设置 overwrite=true。执行前会请求用户批准。",
            Permission::WriteFs,
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目标文件路径（相对默认工作区，或 `工作区id:相对路径`）" },
                    "content": { "type": "string", "description": "完整文件内容" },
                    "overwrite": { "type": "boolean", "description": "文件已存在时是否允许覆盖（默认 false）" },
                    "create_parents": { "type": "boolean", "description": "是否自动创建缺失的父目录（默认 true）" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let raw_path = args::required_str(&args, "path")?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("缺少必填参数「content」（字符串）"))?;
        let overwrite = args::optional_bool(&args, "overwrite", false);
        let create_parents = args::optional_bool(&args, "create_parents", true);

        let resolved = cx.services.workspaces.resolve_writable(&raw_path).map_err(anyhow::Error::msg)?;
        let target = resolved.abs_path.clone();

        if target.is_dir() {
            anyhow::bail!("{} 是目录，不能写入", target.display());
        }
        if target.exists() && !overwrite {
            anyhow::bail!(
                "文件已存在：{}。如需覆盖请设置 overwrite=true，或改用 edit_file 做局部修改",
                target.display()
            );
        }
        let existed = target.exists();

        if let Some(parent) = target.parent() {
            if !parent.exists() {
                if !create_parents {
                    anyhow::bail!("父目录不存在：{}（可设置 create_parents=true 自动创建）", parent.display());
                }
                std::fs::create_dir_all(parent)?;
            }
        }

        cx.ensure_not_cancelled()?;
        atomic_write(&target, content.as_bytes())?;

        let text = format!(
            "{}：{}（{} 字节，工作区 {}）",
            if existed { "已覆盖" } else { "已创建" },
            target.display(),
            content.len(),
            resolved.root_id
        );
        Ok(ToolOutput::text(text.clone()).with_preview(text))
    }
}

/// 精确替换文件中的一段文本
pub struct EditFile;

#[async_trait::async_trait]
impl Tool for EditFile {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "edit_file",
            "编辑文件",
            "把文件中一段精确匹配的文本替换为新文本。默认要求这段文本在文件中唯一出现；需要替换全部出现时设置 replace_all=true。执行前会请求用户批准。",
            Permission::WriteFs,
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目标文件路径" },
                    "old_string": { "type": "string", "description": "要被替换的原文（必须精确匹配，含缩进）" },
                    "new_string": { "type": "string", "description": "替换后的新文本" },
                    "replace_all": { "type": "boolean", "description": "是否替换全部出现（默认 false）" }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let raw_path = args::required_str(&args, "path")?;
        let old_string = args::required_str(&args, "old_string")?;
        let new_string = args
            .get("new_string")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("缺少必填参数「new_string」（字符串）"))?;
        let replace_all = args::optional_bool(&args, "replace_all", false);

        if old_string == new_string {
            anyhow::bail!("old_string 与 new_string 相同，无需修改");
        }

        let resolved = cx.services.workspaces.resolve_writable(&raw_path).map_err(anyhow::Error::msg)?;
        let target = resolved.abs_path.clone();
        if !target.is_file() {
            anyhow::bail!("文件不存在：{}", target.display());
        }

        let bytes = std::fs::read(&target)?;
        if super::fs_read::is_binary(&bytes) {
            anyhow::bail!("{} 是二进制文件，无法编辑", target.display());
        }
        let text = String::from_utf8_lossy(&bytes).to_string();

        let count = text.matches(&old_string).count();
        if count == 0 {
            anyhow::bail!(
                "没有找到要替换的原文。请先用 read_file 确认原文（注意缩进与换行必须完全一致）"
            );
        }
        if count > 1 && !replace_all {
            anyhow::bail!(
                "原文在文件中出现了 {} 次，无法确定替换哪一处。请提供更长的上下文，或设置 replace_all=true",
                count
            );
        }

        let updated = if replace_all {
            text.replace(&old_string, new_string)
        } else {
            text.replacen(&old_string, new_string, 1)
        };

        cx.ensure_not_cancelled()?;
        atomic_write(&target, updated.as_bytes())?;

        let summary = format!(
            "已修改：{}（替换 {} 处，工作区 {}）",
            target.display(),
            if replace_all { count } else { 1 },
            resolved.root_id
        );
        let diff_preview = truncate_text(
            &format!(
                "- {}\n+ {}",
                truncate_text(&old_string, 200).0,
                truncate_text(new_string, 200).0
            ),
            400,
        )
        .0;
        Ok(ToolOutput::text(summary.clone()).with_preview(format!("{}\n{}", summary, diff_preview)))
    }
}

/// 原子写入：临时文件 → fsync → rename（与 config.json 的写入方式一致）
fn atomic_write(target: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法确定目标目录"))?;
    let tmp = dir.join(format!(".konata-tmp-{}", uuid::Uuid::new_v4()));

    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    // Windows 上 std::fs::rename 使用 MOVEFILE_REPLACE_EXISTING，可覆盖既有文件
    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::traits::{DenyAllApprover, NullSink, ToolLimits, ToolServices};
    use crate::config::types::{ToolConfig, ToolMode};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::Duration;

    struct Fixture {
        dir: PathBuf,
        services: ToolServices,
        sink: Arc<NullSink>,
        cancel: Arc<AtomicBool>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str, writable: bool) -> Self {
            let dir = std::env::temp_dir().join(format!("konata-write-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("note.md"), "第一行\n第二行\n第二行\n").unwrap();

            let cfg = ToolConfig::with_single_root(&dir, writable, "测试");
            let set = WorkspaceSet::from_config(&cfg, &dir);
            Self {
                services: ToolServices::minimal(dir.clone(), set, ToolMode::Standard),
                dir,
                sink: Arc::new(NullSink),
                cancel: Arc::new(AtomicBool::new(false)),
            }
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                session_id: "s1",
                stream_id: "st1",
                step: 0,
                cancel: self.cancel.clone(),
                services: &self.services,
                limits: ToolLimits {
                    max_output_bytes: 64 * 1024,
                    call_timeout: Duration::from_secs(5),
                    approval_timeout: Duration::from_secs(5),
                },
                emit: self.sink.clone(),
                approver: Arc::new(DenyAllApprover),
            }
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn writes_new_file_and_creates_parents() {
        let fx = Fixture::new("new", true);
        let cx = fx.ctx();
        let out = block_on(WriteFile.call(
            json!({"path": "sub/dir/a.txt", "content": "你好"}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("已创建"));
        assert_eq!(
            std::fs::read_to_string(fx.dir.join("sub/dir/a.txt")).unwrap(),
            "你好"
        );
        // 不留临时文件
        let leftovers: Vec<_> = std::fs::read_dir(fx.dir.join("sub/dir"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".konata-tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn refuses_overwrite_without_flag() {
        let fx = Fixture::new("overwrite", true);
        let cx = fx.ctx();
        let err = block_on(WriteFile.call(
            json!({"path": "note.md", "content": "覆盖"}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("overwrite=true"), "{err}");

        block_on(WriteFile.call(
            json!({"path": "note.md", "content": "覆盖", "overwrite": true}),
            &cx,
        ))
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(fx.dir.join("note.md")).unwrap(),
            "覆盖"
        );
    }

    #[test]
    fn readonly_workspace_cannot_be_written() {
        let fx = Fixture::new("readonly", false);
        let cx = fx.ctx();
        let err = block_on(WriteFile.call(json!({"path": "x.txt", "content": "x"}), &cx))
            .unwrap_err();
        assert!(err.to_string().contains("只读"), "{err}");
    }

    #[test]
    fn edit_file_replaces_unique_match() {
        let fx = Fixture::new("edit", true);
        let cx = fx.ctx();
        let out = block_on(EditFile.call(
            json!({"path": "note.md", "old_string": "第一行", "new_string": "第 1 行"}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("替换 1 处"));
        assert!(std::fs::read_to_string(fx.dir.join("note.md"))
            .unwrap()
            .starts_with("第 1 行"));
    }

    #[test]
    fn edit_file_requires_unique_match() {
        let fx = Fixture::new("dup", true);
        let cx = fx.ctx();
        let err = block_on(EditFile.call(
            json!({"path": "note.md", "old_string": "第二行", "new_string": "x"}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("出现了 2 次"), "{err}");

        let out = block_on(EditFile.call(
            json!({"path": "note.md", "old_string": "第二行", "new_string": "x", "replace_all": true}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("替换 2 处"));
    }

    #[test]
    fn edit_file_reports_missing_old_string() {
        let fx = Fixture::new("missing", true);
        let cx = fx.ctx();
        let err = block_on(EditFile.call(
            json!({"path": "note.md", "old_string": "不存在的内容", "new_string": "x"}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("没有找到"), "{err}");
    }

    #[test]
    fn edit_file_cannot_escape_workspace() {
        let fx = Fixture::new("escape", true);
        let cx = fx.ctx();
        assert!(block_on(EditFile.call(
            json!({"path": "../../../etc/hosts", "old_string": "a", "new_string": "b"}),
            &cx,
        ))
        .is_err());
    }

    #[test]
    fn edit_file_refuses_sensitive_target() {
        let fx = Fixture::new("sensitive", true);
        std::fs::write(fx.dir.join("config.json"), "{}").unwrap();
        let cx = fx.ctx();
        assert!(block_on(EditFile.call(
            json!({"path": "config.json", "old_string": "{}", "new_string": "[]"}),
            &cx,
        ))
        .is_err());
    }
}
