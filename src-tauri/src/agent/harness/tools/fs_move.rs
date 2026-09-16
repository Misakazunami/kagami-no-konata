use anyhow::Result;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

use crate::agent::harness::snapshot::{capture_before_change, snapshot_note, Capture};
use crate::agent::harness::traits::{
    Permission, Tool, ToolCtx, ToolDescriptor, ToolOutput,
};
use crate::agent::harness::trash::{self, Removal};

use super::args;

/// 单次操作允许涉及的条目数上限（超过就要求先缩小范围）
const MAX_ENTRIES: usize = 5000;
/// 单次操作允许涉及的总字节上限（2 GiB）
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// 路径规模统计（用于审批摘要与限额）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PathStats {
    entries: usize,
    bytes: u64,
    /// 是否因为触到上限而提前停止（意味着实际规模只会更大）
    capped: bool,
}

impl PathStats {
    fn human_bytes(&self) -> String {
        human_bytes(self.bytes)
    }
}

fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// 统计目标规模：文件只算自身，目录递归统计（不跟随符号链接）
///
/// 触到上限即停：审批摘要要在用户等得不耐烦之前给出来，
/// 而"超过上限"本身就是我们要的结论。
fn stat_target(target: &Path) -> Result<PathStats> {
    let metadata = fs::symlink_metadata(target)
        .map_err(|e| anyhow::anyhow!("无法读取 {}：{}", target.display(), e))?;
    if !metadata.is_dir() {
        return Ok(PathStats {
            entries: 1,
            bytes: metadata.len(),
            capped: false,
        });
    }

    let mut entries = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![target.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            // 读不进去的目录不计入，但不因此判定失败：后面的真实操作会报错
            Err(_) => continue,
        };
        for entry in read.flatten() {
            entries += 1;
            let path = entry.path();
            match fs::symlink_metadata(&path) {
                Ok(meta) => {
                    // 只统计普通文件大小：目录本身与符号链接的大小没有意义
                    if meta.is_file() {
                        bytes = bytes.saturating_add(meta.len());
                    } else if meta.is_dir() {
                        stack.push(path);
                    }
                }
                Err(_) => continue,
            }
            if entries > MAX_ENTRIES || bytes > MAX_BYTES {
                return Ok(PathStats {
                    entries,
                    bytes,
                    capped: true,
                });
            }
        }
    }
    Ok(PathStats {
        entries,
        bytes,
        capped: false,
    })
}

/// 规模是否超限（超限时的错误信息统一在这里生成，模型能据此自我修正）
fn ensure_within_limits(stats: &PathStats, what: &str) -> Result<()> {
    if stats.capped || stats.entries > MAX_ENTRIES {
        anyhow::bail!(
            "涉及条目过多（超过 {} 个），已拒绝{}。请先缩小范围（例如先删掉中间产物目录）",
            MAX_ENTRIES,
            what
        );
    }
    if stats.bytes > MAX_BYTES {
        anyhow::bail!(
            "涉及数据过大（超过 {}），已拒绝{}",
            human_bytes(MAX_BYTES),
            what
        );
    }
    Ok(())
}

/// 移动/复制一个路径（只在**跨卷**时退化为"复制 + 删除源"）
///
/// 历史实现把任何 `rename` 失败（权限、占用等）都当成跨卷，会导致
/// "明明只是没权限，却去复制一份再删源"的危险行为；这里只对
/// `ErrorKind::CrossesDevices`（Unix EXDEV / Windows ERROR_NOT_SAME_DEVICE）
/// 退化，其余错误如实上报。
fn move_any(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            let target_existed = to.exists();
            if let Err(copy_err) = copy_any(from, to) {
                // 复制失败：清掉可能已经写了一半的新目标，源保持不动
                if !target_existed {
                    let _ = remove_any(to);
                }
                return Err(copy_err);
            }
            remove_any(from)?;
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!(
            "移动失败（{} → {}）：{}",
            from.display(),
            to.display(),
            e
        )),
    }
}

fn is_cross_device(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::CrossesDevices
        // 老平台/特殊文件系统兜底：Unix 的 EXDEV
        || e.raw_os_error() == Some(18)
}

fn copy_any(from: &Path, to: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(from)
        .map_err(|e| anyhow::anyhow!("无法读取 {}：{}", from.display(), e))?;
    if metadata.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_any(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_file_atomically(from, to)
    }
}

/// 文件复制：先写同目录临时文件，再 rename 覆盖目标
///
/// `fs::copy` 会先截断目标再写，中途失败（磁盘满/IO 错误）会把原有内容
/// 变成半截文件；临时文件 + rename 保证"要么旧内容、要么完整新内容"。
fn copy_file_atomically(from: &Path, to: &Path) -> Result<()> {
    let parent = to
        .parent()
        .ok_or_else(|| anyhow::anyhow!("无法确定目标目录：{}", to.display()))?;
    let tmp = parent.join(format!(".konata-copy-{}", uuid::Uuid::new_v4()));
    if let Err(e) = fs::copy(from, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("复制失败（{}）：{}", from.display(), e));
    }
    if let Err(e) = fs::rename(&tmp, to) {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow::anyhow!(
            "复制完成但无法替换目标（{}）：{}",
            to.display(),
            e
        ));
    }
    Ok(())
}

fn remove_any(target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(target)?;
    if metadata.is_dir() {
        fs::remove_dir_all(target).map_err(|e| anyhow::anyhow!(e))
    } else {
        fs::remove_file(target).map_err(|e| anyhow::anyhow!(e))
    }
}

/// 若目标已存在且允许覆盖，先给目标做快照（否则回滚拿不回被覆盖的内容）
fn capture_existing(
    cx: &ToolCtx<'_>,
    root_id: &str,
    rel: &Path,
    abs: &Path,
) -> Option<Capture> {
    if !abs.exists() {
        return None;
    }
    capture_before_change(
        cx.services,
        cx.session_id,
        cx.stream_id,
        root_id,
        rel,
        abs,
    )
}

// ─── delete_path ─────────────────────────────────────────

/// 删除工作区内的文件或目录
pub struct DeletePath;

#[async_trait::async_trait]
impl Tool for DeletePath {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "delete_path",
            "删除文件或目录",
            "删除工作区内的文件或目录。默认移入系统回收站（可以找回），只有显式设置 permanent=true 才会永久删除（审批弹窗里会标红）。删除目录必须设置 recursive=true。执行前会请求用户批准。",
            Permission::WriteFs,
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "要删除的路径（相对默认工作区，或 `工作区id:相对路径`）" },
                    "recursive": { "type": "boolean", "description": "删除目录时必须为 true（默认 false，避免误删整个目录）" },
                    "permanent": { "type": "boolean", "description": "是否永久删除（默认 false = 移入回收站，可找回）" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    fn approval_summary(&self, args: &Value, cx: &ToolCtx<'_>) -> Option<String> {
        let raw = args.get("path").and_then(|v| v.as_str())?;
        let permanent = args
            .get("permanent")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let resolved = cx.services.workspaces.resolve_writable(raw).ok()?;
        let target = resolved.abs_path.clone();
        if target == resolved.root_path {
            return Some("⚠ 这是工作区根目录，删除会被拒绝".to_string());
        }
        let method = if permanent {
            "永久删除（无法从回收站找回）"
        } else {
            "移入系统回收站（可以找回）"
        };
        match stat_target(&target) {
            Ok(stats) => {
                let kind = if target.is_dir() { "目录" } else { "文件" };
                let head = if permanent {
                    format!("将永久删除{}：{}", kind, resolved.rel.display())
                } else {
                    format!("将删除{}：{}", kind, resolved.rel.display())
                };
                let mut out = format!(
                    "{}\n规模：{} 个条目，{} 字节（{}）",
                    head,
                    stats.entries,
                    stats.bytes,
                    stats.human_bytes()
                );
                out.push_str(&format!("\n方式：{}", method));
                if stats.capped || stats.entries > MAX_ENTRIES || stats.bytes > MAX_BYTES {
                    out.push_str(&format!(
                        "\n⚠ 超出单次操作上限（最多 {} 个条目 / {}），这次调用会被拒绝",
                        MAX_ENTRIES,
                        human_bytes(MAX_BYTES)
                    ));
                }
                Some(out)
            }
            Err(e) => Some(format!("⚠ {}（删除会被拒绝）", e)),
        }
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let raw_path = args::required_str(&args, "path")?;
        let recursive = args::optional_bool(&args, "recursive", false);
        let permanent = args::optional_bool(&args, "permanent", false);

        let resolved = cx
            .services
            .workspaces
            .resolve_writable(&raw_path)
            .map_err(anyhow::Error::msg)?;
        let target = resolved.abs_path.clone();

        // 工作区根目录永远不能删（否则整个沙箱连同用户的仓库一起没了）
        if target == resolved.root_path {
            anyhow::bail!("不能删除工作区根目录：{}", target.display());
        }
        let metadata = fs::symlink_metadata(&target)
            .map_err(|e| anyhow::anyhow!("无法删除 {}：{}", target.display(), e))?;

        let stats = stat_target(&target)?;
        ensure_within_limits(&stats, "删除")?;
        if metadata.is_dir() && !recursive {
            anyhow::bail!(
                "{} 是目录：删除目录必须显式设置 recursive=true（共 {} 个条目）",
                target.display(),
                stats.entries
            );
        }

        // 文件：删除前备份，这样即使进了回收站也能在应用里一键回滚
        let capture = if metadata.is_file() {
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
        let removal = if permanent {
            trash::remove_permanently(&target)?;
            Removal::Permanent
        } else {
            trash::move_to_trash(&target)?;
            Removal::Trashed
        };

        let kind = if metadata.is_dir() { "目录" } else { "文件" };
        let body = format!(
            "已删除{}：{}（{}，{} 个条目）\n方式：{}{}",
            kind,
            resolved.rel.display(),
            stats.human_bytes(),
            stats.entries,
            removal.label(),
            snapshot_note(capture.as_ref())
        );
        let preview = format!(
            "删除{} {} · {}",
            kind,
            resolved.rel.display(),
            removal.label()
        );
        Ok(ToolOutput::text(body).with_preview(preview))
    }
}

// ─── move_path ───────────────────────────────────────────

/// 在工作区内移动/重命名
pub struct MovePath;

#[async_trait::async_trait]
impl Tool for MovePath {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "move_path",
            "移动或重命名",
            "在工作区内移动或重命名文件/目录（两端都必须在可写工作区内）。目标已存在时必须设置 overwrite=true，此时会先备份被覆盖的内容。执行前会请求用户批准。",
            Permission::WriteFs,
            json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "源路径" },
                    "to": { "type": "string", "description": "目标路径（目录不存在时用 create_parents=true 自动创建父目录）" },
                    "overwrite": { "type": "boolean", "description": "目标已存在时是否覆盖（默认 false）" },
                    "create_parents": { "type": "boolean", "description": "目标父目录不存在时是否自动创建（默认 false）" }
                },
                "required": ["from", "to"],
                "additionalProperties": false
            }),
        )
    }

    fn approval_summary(&self, args: &Value, cx: &ToolCtx<'_>) -> Option<String> {
        let from_raw = args.get("from").and_then(|v| v.as_str())?;
        let to_raw = args.get("to").and_then(|v| v.as_str())?;
        let from = cx.services.workspaces.resolve_writable(from_raw).ok()?;
        let to = cx.services.workspaces.resolve_writable(to_raw).ok()?;
        let overwrite_note = if to.abs_path.exists() {
            "\n⚠ 目标已存在，将被覆盖（覆盖前会先备份）"
        } else {
            ""
        };
        Some(format!(
            "将把 {} 移动到 {}{}",
            from.rel.display(),
            to.rel.display(),
            overwrite_note
        ))
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let from_raw = args::required_str(&args, "from")?;
        let to_raw = args::required_str(&args, "to")?;
        let overwrite = args::optional_bool(&args, "overwrite", false);
        let create_parents = args::optional_bool(&args, "create_parents", false);

        let from = cx
            .services
            .workspaces
            .resolve_writable(&from_raw)
            .map_err(anyhow::Error::msg)?;
        let to = cx
            .services
            .workspaces
            .resolve_writable(&to_raw)
            .map_err(anyhow::Error::msg)?;

        if from.abs_path == to.abs_path {
            anyhow::bail!("源与目标是同一个路径：{}", from.abs_path.display());
        }
        if from.abs_path == from.root_path {
            anyhow::bail!("不能移动工作区根目录：{}", from.abs_path.display());
        }
        if !from.abs_path.exists() {
            anyhow::bail!("源路径不存在：{}", from.abs_path.display());
        }
        // 目录不能移动到自己内部（会变成无限递归/丢失）
        if to.abs_path.starts_with(&from.abs_path) {
            anyhow::bail!(
                "不能把 {} 移动到它自己的子路径 {} 下",
                from.rel.display(),
                to.rel.display()
            );
        }
        if to.abs_path.is_dir() {
            anyhow::bail!("目标 {} 是已存在的目录", to.abs_path.display());
        }
        if to.abs_path.exists() && !overwrite {
            anyhow::bail!(
                "目标已存在：{}。如需覆盖请设置 overwrite=true",
                to.abs_path.display()
            );
        }

        let stats = stat_target(&from.abs_path)?;
        ensure_within_limits(&stats, "移动")?;

        if let Some(parent) = to.abs_path.parent() {
            if !parent.exists() {
                if !create_parents {
                    anyhow::bail!(
                        "目标父目录不存在：{}（可设置 create_parents=true 自动创建）",
                        parent.display()
                    );
                }
                fs::create_dir_all(parent)?;
            }
        }

        // 备份：源（回滚时可放回原位）+ 将被覆盖的目标
        let source_capture = capture_before_change(
            cx.services,
            cx.session_id,
            cx.stream_id,
            &from.root_id,
            &from.rel,
            &from.abs_path,
        );
        let target_capture = if to.abs_path.exists() {
            capture_existing(
                cx,
                &to.root_id,
                &to.rel,
                &to.abs_path,
            )
        } else {
            None
        };

        cx.ensure_not_cancelled()?;
        move_any(&from.abs_path, &to.abs_path)?;

        let mut notes = snapshot_note(source_capture.as_ref());
        if target_capture.is_some() {
            notes.push_str("；被覆盖的目标也已备份");
        }
        if from.abs_path.is_dir() {
            notes.push_str("\n（注意：回滚会把源放回原位，但不会删除已经移动过去的副本）");
        }

        let body = format!(
            "已移动：{} → {}（{} 个条目，{}）{}",
            from.rel.display(),
            to.rel.display(),
            stats.entries,
            stats.human_bytes(),
            notes
        );
        let preview = format!("移动 {} → {}", from.rel.display(), to.rel.display());
        Ok(ToolOutput::text(body).with_preview(preview))
    }
}

// ─── copy_path ───────────────────────────────────────────

/// 在工作区内复制文件/目录
pub struct CopyPath;

#[async_trait::async_trait]
impl Tool for CopyPath {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            "copy_path",
            "复制文件或目录",
            "在工作区内复制文件或目录（两端都必须在可写工作区内）。复制目录必须设置 recursive=true；目标已存在时必须设置 overwrite=true。执行前会请求用户批准。",
            Permission::WriteFs,
            json!({
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "源路径" },
                    "to": { "type": "string", "description": "目标路径" },
                    "recursive": { "type": "boolean", "description": "复制目录时必须为 true" },
                    "overwrite": { "type": "boolean", "description": "目标已存在时是否覆盖（默认 false）" },
                    "create_parents": { "type": "boolean", "description": "目标父目录不存在时是否自动创建（默认 false）" }
                },
                "required": ["from", "to"],
                "additionalProperties": false
            }),
        )
    }

    fn approval_summary(&self, args: &Value, cx: &ToolCtx<'_>) -> Option<String> {
        let from_raw = args.get("from").and_then(|v| v.as_str())?;
        let to_raw = args.get("to").and_then(|v| v.as_str())?;
        let from = cx.services.workspaces.resolve_writable(from_raw).ok()?;
        let to = cx.services.workspaces.resolve_writable(to_raw).ok()?;
        let stats = stat_target(&from.abs_path).ok();
        let size = stats
            .map(|s| format!("（{} 个条目，{}）", s.entries, s.human_bytes()))
            .unwrap_or_default();
        let overwrite_note = if to.abs_path.exists() {
            "\n⚠ 目标已存在，将被覆盖（覆盖前会先备份）"
        } else {
            ""
        };
        Some(format!(
            "将把 {} 复制到 {}{}{}",
            from.rel.display(),
            to.rel.display(),
            size,
            overwrite_note
        ))
    }

    async fn call(&self, args: Value, cx: &ToolCtx<'_>) -> Result<ToolOutput> {
        cx.ensure_not_cancelled()?;
        let from_raw = args::required_str(&args, "from")?;
        let to_raw = args::required_str(&args, "to")?;
        let recursive = args::optional_bool(&args, "recursive", false);
        let overwrite = args::optional_bool(&args, "overwrite", false);
        let create_parents = args::optional_bool(&args, "create_parents", false);

        let from = cx
            .services
            .workspaces
            .resolve_writable(&from_raw)
            .map_err(anyhow::Error::msg)?;
        let to = cx
            .services
            .workspaces
            .resolve_writable(&to_raw)
            .map_err(anyhow::Error::msg)?;

        if from.abs_path == to.abs_path {
            anyhow::bail!("源与目标是同一个路径：{}", from.abs_path.display());
        }
        if !from.abs_path.exists() {
            anyhow::bail!("源路径不存在：{}", from.abs_path.display());
        }
        if to.abs_path.starts_with(&from.abs_path) {
            anyhow::bail!(
                "不能把 {} 复制到它自己的子路径 {} 下",
                from.rel.display(),
                to.rel.display()
            );
        }
        let is_dir = from.abs_path.is_dir();
        if is_dir && !recursive {
            anyhow::bail!("{} 是目录：复制目录必须显式设置 recursive=true", from.abs_path.display());
        }
        if !is_dir && to.abs_path.is_dir() {
            anyhow::bail!("目标 {} 是目录，不能作为文件的目标路径", to.abs_path.display());
        }
        if to.abs_path.exists() && !overwrite {
            anyhow::bail!(
                "目标已存在：{}。如需覆盖请设置 overwrite=true",
                to.abs_path.display()
            );
        }

        let stats = stat_target(&from.abs_path)?;
        ensure_within_limits(&stats, "复制")?;

        if let Some(parent) = to.abs_path.parent() {
            if !parent.exists() {
                if !create_parents {
                    anyhow::bail!(
                        "目标父目录不存在：{}（可设置 create_parents=true 自动创建）",
                        parent.display()
                    );
                }
                fs::create_dir_all(parent)?;
            }
        }

        // 目标会被覆盖时先备份它（复制本身不动源文件，无需备份源）
        let target_existed = to.abs_path.exists();
        let target_capture = if target_existed {
            capture_existing(cx, &to.root_id, &to.rel, &to.abs_path)
        } else {
            None
        };
        if to.abs_path.exists() && to.abs_path.is_dir() {
            // 目录覆盖语义在各平台不一致，明确拒绝而不是猜
            anyhow::bail!("目标 {} 已存在且是目录，拒绝覆盖目录", to.abs_path.display());
        }

        cx.ensure_not_cancelled()?;
        if let Err(e) = copy_any(&from.abs_path, &to.abs_path) {
            // 目录复制不是原子的：失败时把"本次新建的半个目标"清掉，
            // 避免工作区里留下一个看似完整的副本（已存在的目标由原子文件复制保护）
            if !target_existed {
                let _ = remove_any(&to.abs_path);
            }
            return Err(e);
        }

        let body = format!(
            "已复制：{} → {}（{} 个条目，{}）{}",
            from.rel.display(),
            to.rel.display(),
            stats.entries,
            stats.human_bytes(),
            match &target_capture {
                Some(capture) => snapshot_note(Some(capture)),
                None => String::new(),
            }
        );
        let preview = format!("复制 {} → {}", from.rel.display(), to.rel.display());
        Ok(ToolOutput::text(body).with_preview(preview))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::harness::jail::WorkspaceSet;
    use crate::agent::harness::snapshot::SnapshotStore;
    use crate::agent::harness::traits::{
        DenyAllApprover, ToolLimits, ToolServices,
    };
    use crate::config::types::{ToolConfig, ToolMode};
    use crate::store::chat_store::ChatStore;
    use crate::store::db;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct Fixture {
        dir: PathBuf,
        services: ToolServices,
        cancel: Arc<AtomicBool>,
        snapshots: Arc<SnapshotStore>,
        session_id: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("konata-mv-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let conn = db::init_db(&dir).unwrap();
            let store = Arc::new(Mutex::new(ChatStore::new(conn)));
            let session = store
                .lock()
                .unwrap()
                .create_session("konata-default", "测试", None, None, None)
                .unwrap();
            let cfg = ToolConfig::with_single_root(&dir, true, "测试");
            let set = WorkspaceSet::from_config(&cfg, &dir);
            let snapshots = Arc::new(SnapshotStore::new(&dir, store.clone()));
            let mut services = ToolServices::minimal(dir.clone(), set, ToolMode::Full);
            services.chat_store = Some(store.clone());
            services.snapshots = Some(snapshots.clone());
            Self {
                dir,
                services,
                cancel: Arc::new(AtomicBool::new(false)),
                snapshots,
                session_id: session.id,
            }
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
                    call_timeout: Duration::from_secs(10),
                    approval_timeout: Duration::from_secs(5),
                },
                emit: Arc::new(crate::agent::harness::NullSink),
                approver: Arc::new(DenyAllApprover),
            }
        }

        fn write(&self, name: &str, content: &str) -> PathBuf {
            let path = self.dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, content).unwrap();
            path
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    // ─── delete_path ───

    #[test]
    fn delete_refuses_workspace_root() {
        let fx = Fixture::new("root");
        let cx = fx.ctx();
        let err = block_on(DeletePath.call(json!({"path": "."}), &cx)).unwrap_err();
        assert!(err.to_string().contains("根目录"), "{err}");
    }

    #[test]
    fn delete_refuses_directory_without_recursive() {
        let fx = Fixture::new("norec");
        fx.write("sub/a.txt", "x");
        let cx = fx.ctx();
        let err = block_on(DeletePath.call(json!({"path": "sub"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("recursive"), "{err}");
        assert!(fx.dir.join("sub/a.txt").exists(), "被拒绝时不能删除任何东西");
    }

    #[test]
    fn delete_denies_sensitive_paths_via_jail() {
        let fx = Fixture::new("deny");
        fx.write("config.json", "{}");
        let cx = fx.ctx();
        let err = block_on(DeletePath.call(json!({"path": "config.json"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("拒绝") || err.to_string().contains("敏感"), "{err}");
        assert!(fx.dir.join("config.json").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn delete_permanent_removes_and_backs_up() {
        let fx = Fixture::new("perm");
        fx.write("note.txt", "重要内容");
        let cx = fx.ctx();
        let out = block_on(DeletePath.call(
            json!({"path": "note.txt", "permanent": true}),
            &cx,
        ))
        .unwrap();
        assert!(!fx.dir.join("note.txt").exists());
        assert!(out.content.contains("永久删除"), "{}", out.content);

        // 删除前已备份 → 可以回滚
        let info = fx.snapshots.info(&fx.session_id, "st1");
        assert_eq!(info.files, 1);
        let report = fx.snapshots.restore("st1", &fx.session_id, &fx.services.workspaces);
        assert_eq!(report.restored, 1, "{report:?}");
        assert_eq!(
            std::fs::read_to_string(fx.dir.join("note.txt")).unwrap(),
            "重要内容"
        );
    }

    #[test]
    fn delete_approval_summary_reports_size_and_method() {
        let fx = Fixture::new("summary");
        fx.write("data.txt", "12345");
        let cx = fx.ctx();
        let summary = DeletePath
            .approval_summary(&json!({"path": "data.txt"}), &cx)
            .expect("必须给出摘要");
        assert!(summary.contains("data.txt"), "{summary}");
        assert!(summary.contains("回收站"), "{summary}");

        let permanent = DeletePath
            .approval_summary(&json!({"path": "data.txt", "permanent": true}), &cx)
            .unwrap();
        assert!(permanent.contains("永久删除"), "{permanent}");

        let root = DeletePath
            .approval_summary(&json!({"path": "."}), &cx)
            .unwrap();
        assert!(root.contains("会被拒绝"), "{root}");
    }

    // ─── move_path ───

    #[test]
    fn move_renames_and_backs_up_source() {
        let fx = Fixture::new("move");
        fx.write("old.txt", "内容");
        let cx = fx.ctx();
        let out = block_on(MovePath.call(
            json!({"from": "old.txt", "to": "new.txt"}),
            &cx,
        ))
        .unwrap();
        assert!(!fx.dir.join("old.txt").exists());
        assert_eq!(std::fs::read_to_string(fx.dir.join("new.txt")).unwrap(), "内容");
        assert!(out.content.contains("已移动"), "{}", out.content);

        // 回滚把源放回原位
        fx.snapshots.restore("st1", &fx.session_id, &fx.services.workspaces);
        assert_eq!(
            std::fs::read_to_string(fx.dir.join("old.txt")).unwrap(),
            "内容"
        );
    }

    #[test]
    fn move_rejects_missing_source_and_self_nesting() {
        let fx = Fixture::new("movebad");
        fx.write("dir/inner.txt", "x");
        let cx = fx.ctx();
        let err = block_on(MovePath.call(json!({"from": "ghost", "to": "x"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("不存在"), "{err}");

        let err = block_on(MovePath.call(json!({"from": "dir", "to": "dir/sub"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("子路径"), "{err}");
    }

    #[test]
    fn move_requires_overwrite_for_existing_target_and_backs_it_up() {
        let fx = Fixture::new("moveover");
        fx.write("a.txt", "A");
        fx.write("b.txt", "B");
        let cx = fx.ctx();

        let err = block_on(MovePath.call(json!({"from": "a.txt", "to": "b.txt"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("overwrite"), "{err}");

        block_on(MovePath.call(
            json!({"from": "a.txt", "to": "b.txt", "overwrite": true}),
            &cx,
        ))
        .unwrap();
        assert_eq!(std::fs::read_to_string(fx.dir.join("b.txt")).unwrap(), "A");
        // 目标与源各有一份备份
        assert_eq!(fx.snapshots.info(&fx.session_id, "st1").files, 2);
    }

    #[test]
    fn move_approval_summary_mentions_overwrite() {
        let fx = Fixture::new("movesum");
        fx.write("a.txt", "A");
        fx.write("b.txt", "B");
        let cx = fx.ctx();
        let summary = MovePath
            .approval_summary(&json!({"from": "a.txt", "to": "b.txt"}), &cx)
            .unwrap();
        assert!(summary.contains("将被覆盖"), "{summary}");
    }

    // ─── copy_path ───

    #[test]
    fn copy_creates_duplicate_and_keeps_source() {
        let fx = Fixture::new("copy");
        fx.write("src.txt", "内容");
        let cx = fx.ctx();
        block_on(CopyPath.call(json!({"from": "src.txt", "to": "copy.txt"}), &cx)).unwrap();
        assert!(fx.dir.join("src.txt").exists());
        assert_eq!(std::fs::read_to_string(fx.dir.join("copy.txt")).unwrap(), "内容");
        // 复制不动源文件 → 没有备份记录（没有东西被覆盖）
        assert_eq!(fx.snapshots.info(&fx.session_id, "st1").files, 0);
    }

    #[test]
    fn copy_directory_requires_recursive() {
        let fx = Fixture::new("copydir");
        fx.write("d/f.txt", "x");
        let cx = fx.ctx();
        let err = block_on(CopyPath.call(json!({"from": "d", "to": "e"}), &cx)).unwrap_err();
        assert!(err.to_string().contains("recursive"), "{err}");

        block_on(CopyPath.call(
            json!({"from": "d", "to": "e", "recursive": true, "create_parents": true}),
            &cx,
        ))
        .unwrap();
        assert_eq!(std::fs::read_to_string(fx.dir.join("e/f.txt")).unwrap(), "x");
    }

    #[test]
    fn copy_creates_parent_directories_only_when_asked() {
        let fx = Fixture::new("copyparent");
        fx.write("f.txt", "x");
        let cx = fx.ctx();
        let err = block_on(CopyPath.call(json!({"from": "f.txt", "to": "deep/nested/f.txt"}), &cx))
            .unwrap_err();
        assert!(err.to_string().contains("create_parents"), "{err}");

        block_on(CopyPath.call(
            json!({"from": "f.txt", "to": "deep/nested/f.txt", "create_parents": true}),
            &cx,
        ))
        .unwrap();
        assert!(fx.dir.join("deep/nested/f.txt").exists());
    }

    #[test]
    fn move_to_outside_workspace_is_rejected() {
        let fx = Fixture::new("escape");
        fx.write("f.txt", "x");
        let cx = fx.ctx();
        let outside = if cfg!(windows) {
            "C:\\Windows\\Temp\\f.txt"
        } else {
            "/tmp/konata-should-not-exist.txt"
        };
        let err = block_on(MovePath.call(
            json!({"from": "f.txt", "to": outside}),
            &cx,
        ))
        .unwrap_err();
        assert!(err.to_string().contains("工作区"), "{err}");
        assert!(fx.dir.join("f.txt").exists());
    }

    #[test]
    fn tools_are_write_fs_and_need_approval() {
        for descriptor in [
            DeletePath.descriptor(),
            MovePath.descriptor(),
            CopyPath.descriptor(),
        ] {
            assert_eq!(descriptor.permission, Permission::WriteFs);
            assert!(descriptor.permission.requires_approval());
            assert!(descriptor.permission.visible_in(ToolMode::Standard));
            assert!(!descriptor.permission.visible_in(ToolMode::ReadOnly));
        }
    }

    #[test]
    fn stat_target_counts_entries_and_bytes() {
        let fx = Fixture::new("stat");
        fx.write("dir/a.txt", "12345");
        fx.write("dir/sub/b.txt", "123");
        let stats = stat_target(&fx.dir.join("dir")).unwrap();
        assert_eq!(stats.bytes, 8);
        assert!(!stats.capped);
        // 目录本身 + 子目录 + 两个文件
        assert!(stats.entries >= 3, "{stats:?}");
    }
}
