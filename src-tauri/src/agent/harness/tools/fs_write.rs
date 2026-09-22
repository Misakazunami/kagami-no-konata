use anyhow::Result;
use serde_json::{json, Value};
use std::io::Write;

use crate::agent::harness::snapshot::{capture_before_change, snapshot_note};
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

        // 覆盖前备份原文件：这是"敢让模型改代码"的前提（界面上可一键回滚）
        let capture = if existed {
            capture_before_change(
                cx.services,
                cx.session_id,
                cx.stream_id,
                &resolved.root_id,
                &resolved.rel,
                &target,
            )
        } else {
            None
        };

        cx.ensure_not_cancelled()?;
        atomic_write(&target, content.as_bytes())?;

        let mut text = format!(
            "{}：{}（{} 字节，工作区 {}）",
            if existed { "已覆盖" } else { "已创建" },
            target.display(),
            content.len(),
            resolved.root_id
        );
        if existed {
            text.push_str(&snapshot_note(capture.as_ref()));
        } else {
            // 新建文件没有"原内容"可备份，回滚也只会恢复被覆盖的文件：
            // 必须如实说明，否则用户会以为"回滚"会把这个新文件也删掉
            text.push_str(
                "\n（本次为新建文件，没有可回滚的原内容；如需撤销请手动删除该文件）",
            );
        }
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

        let metadata = std::fs::metadata(&target)?;
        if metadata.len() > super::fs_read::MAX_TEXT_FILE_BYTES {
            anyhow::bail!(
                "文件过大（{} MB，上限 {} MB），无法整体编辑：请改用脚本或分片处理",
                metadata.len() / 1024 / 1024,
                super::fs_read::MAX_TEXT_FILE_BYTES / 1024 / 1024
            );
        }
        let bytes = std::fs::read(&target)?;
        if super::fs_read::is_binary(&bytes) {
            anyhow::bail!("{} 是二进制文件，无法编辑", target.display());
        }
        // 必须**严格**按 UTF-8 解码：`from_utf8_lossy` 会把 GBK/Latin-1 等编码里的
        // 非法字节替换成 U+FFFD，随后整体写回会把整个文件永久写成乱码
        // （中文 Windows 上 GBK 的 .txt/.csv/.c 很常见）。
        let text = String::from_utf8(bytes).map_err(|_| {
            anyhow::anyhow!(
                "{} 不是 UTF-8 编码（可能是 GBK/GB18030 等）。为避免写坏文件已拒绝编辑，请先转换为 UTF-8",
                target.display()
            )
        })?;

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

        let capture = capture_before_change(
            cx.services,
            cx.session_id,
            cx.stream_id,
            &resolved.root_id,
            &resolved.rel,
            &target,
        );

        cx.ensure_not_cancelled()?;
        atomic_write(&target, updated.as_bytes())?;

        let summary = format!(
            "已修改：{}（替换 {} 处，工作区 {}）{}",
            target.display(),
            if replace_all { count } else { 1 },
            resolved.root_id,
            snapshot_note(capture.as_ref())
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
///
/// 覆盖已存在的文件时**保留原权限位**：临时文件由 `File::create` 新建
/// （默认 0644 & umask），rename 会整体替换 inode，若不显式回写，0600 的
/// 私密文件会变宽松、可执行脚本会丢可执行位。
fn atomic_write(target: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法确定目标目录"))?;
    let tmp = dir.join(format!(".konata-tmp-{}", uuid::Uuid::new_v4()));
    let original_permissions = std::fs::metadata(target).ok().map(|m| m.permissions());

    {
        let mut file = std::fs::File::create(&tmp)?;
        if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            // 磁盘写满/IO 错误：清掉半截临时文件，不留在用户工作区
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
    }
    if let Some(permissions) = original_permissions {
        let _ = std::fs::set_permissions(&tmp, permissions);
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
        session_id: String,
        snapshots: Option<Arc<crate::agent::harness::SnapshotStore>>,
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
                session_id: "s1".to_string(),
                snapshots: None,
            }
        }

        /// 挂上一个真实的快照服务（覆盖/删除类改动因此可回滚）
        fn with_snapshots(mut self, tag: &str) -> Self {
            let conn = crate::store::db::init_db(&self.dir).unwrap();
            let store = Arc::new(std::sync::Mutex::new(crate::store::chat_store::ChatStore::new(conn)));
            let session = store
                .lock()
                .unwrap()
                .create_session("konata-default", tag, None, None, None)
                .unwrap();
            let snapshots = Arc::new(crate::agent::harness::SnapshotStore::new(
                &self.dir,
                store.clone(),
            ));
            self.services.chat_store = Some(store);
            self.services.snapshots = Some(snapshots.clone());
            self.session_id = session.id;
            self.snapshots = Some(snapshots);
            self
        }

        fn ctx(&self) -> ToolCtx<'_> {
            ToolCtx {
                session_id: &self.session_id,
                stream_id: "st1",
                call_id: "c1",
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

    /// 新建文件没有原内容可回滚：文案必须如实说明，不能套用"未启用快照"的兜底
    #[test]
    fn new_file_note_explains_rollback_scope() {
        let fx = Fixture::new("new-note", true).with_snapshots("快照");
        let cx = fx.ctx();
        let out = block_on(WriteFile.call(
            json!({"path": "brand-new.txt", "content": "x"}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("已创建"), "{}", out.content);
        assert!(out.content.contains("新建文件"), "{}", out.content);
        assert!(
            !out.content.contains("未启用文件快照"),
            "不能误导成快照未启用：{}",
            out.content
        );
    }

    #[test]
    fn overwrite_keeps_a_restorable_backup() {
        let fx = Fixture::new("snap", true).with_snapshots("快照");
        let cx = fx.ctx();
        let snapshots = fx.snapshots.clone().unwrap();
        let session_id = fx.session_id.clone();

        let out = block_on(WriteFile.call(
            json!({"path": "note.md", "content": "全新内容", "overwrite": true}),
            &cx,
        ))
        .unwrap();
        assert!(out.content.contains("已覆盖"), "{}", out.content);
        assert!(out.content.contains("回滚"), "必须告知用户可回滚：{}", out.content);

        let info = snapshots.info(&session_id, "st1");
        assert_eq!(info.files, 1, "覆盖前必须备份");
        assert!(info.bytes > 0);

        // 回滚后回到原内容
        let report = snapshots.restore("st1", &session_id, &fx.services.workspaces);
        assert_eq!(report.restored, 1, "{report:?}");
        assert_eq!(
            std::fs::read_to_string(fx.dir.join("note.md")).unwrap(),
            "第一行\n第二行\n第二行\n"
        );
    }

    #[test]
    fn edit_file_backs_up_before_modifying() {
        let fx = Fixture::new("snapedit", true).with_snapshots("快照");
        let cx = fx.ctx();
        let snapshots = fx.snapshots.clone().unwrap();
        let session_id = fx.session_id.clone();

        block_on(EditFile.call(
            json!({"path": "note.md", "old_string": "第二行", "new_string": "改过了", "replace_all": true}),
            &cx,
        ))
        .unwrap();
        assert_eq!(snapshots.info(&session_id, "st1").files, 1);

        snapshots.restore("st1", &session_id, &fx.services.workspaces);
        assert_eq!(
            std::fs::read_to_string(fx.dir.join("note.md")).unwrap(),
            "第一行\n第二行\n第二行\n"
        );
    }

    #[test]
    fn edit_file_refuses_non_utf8_instead_of_corrupting() {
        let fx = Fixture::new("gbk", true);
        // GBK 编码的「测试」：合法文本但不是合法 UTF-8
        let gbk = vec![0xB2u8, 0xE2, 0xCA, 0xD4, b'\n'];
        std::fs::write(fx.dir.join("gbk.txt"), &gbk).unwrap();

        let cx = fx.ctx();
        let err = block_on(EditFile.call(
            json!({"path": "gbk.txt", "old_string": "x", "new_string": "y"}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "{err}");
        // 关键：文件必须一个字节都没动，而不是被 U+FFFD 替换后写回
        assert_eq!(std::fs::read(fx.dir.join("gbk.txt")).unwrap(), gbk);
    }

    #[test]
    fn edit_file_refuses_oversized_files() {
        let fx = Fixture::new("huge-edit", true);
        let path = fx.dir.join("huge.txt");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(crate::agent::harness::tools::fs_read::MAX_TEXT_FILE_BYTES + 1)
            .unwrap();
        drop(file);

        let cx = fx.ctx();
        let err = block_on(EditFile.call(
            json!({"path": "huge.txt", "old_string": "a", "new_string": "b"}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("过大"), "{err}");
    }

    /// 覆盖写入必须保留原权限位（0600 不能变 0644、可执行位不能丢）
    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let fx = Fixture::new("perms", true);
        let path = fx.dir.join("note.md");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let cx = fx.ctx();
        block_on(WriteFile.call(
            json!({"path": "note.md", "content": "新内容", "overwrite": true}),
            &cx,
        ))
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "覆盖写入必须保留原权限位");
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
