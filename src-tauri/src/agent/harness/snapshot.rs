//! 工作区改动快照与回滚
//!
//! 目的：让"让模型动手改文件"这件事**可撤销**。写类工具在动手之前把被覆盖/
//! 被删除/被移动的**文件**原样备份到 `{app_data_dir}/snapshots/{stream_id}/`，
//! 索引写进 `workspace_snapshots`；用户在界面上看到「本轮改动了 N 个文件 ·
//! 回滚」就能一键把这一轮生成造成的文件改动全部还原。
//!
//! 边界（刻意保守）：
//! - **只备份文件**，不备份目录树（一棵 `node_modules` 会让快照变成几百 MB）；
//!   目录级改动靠删除类操作的回收站兜底；
//! - 单文件上限 4 MB、单次生成上限 64 MB，超限时**跳过并如实告知**，
//!   绝不静默假装备份成功；
//! - 备份是按 stream 分目录的，`prune` 按天数回收磁盘。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use uuid::Uuid;

use super::jail::WorkspaceSet;
use crate::store::chat_store::{ChatStore, SnapshotRow};

/// 单文件备份上限（超过就不备份，避免把磁盘吃满）
pub const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// 单次生成（stream）的备份总量上限
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// 备份默认保留天数
pub const KEEP_DAYS: i64 = 7;

/// 一次备份尝试的结果
#[derive(Debug, Clone)]
pub enum Capture {
    Captured { bytes: u64 },
    /// 跳过备份的原因（必须原样写进工具结果，不能静默）
    Skipped { reason: String },
}

impl Capture {
    pub fn is_captured(&self) -> bool {
        matches!(self, Capture::Captured { .. })
    }
}

/// 某个 stream 的快照概况（界面用它决定要不要显示"回滚"）
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub session_id: String,
    pub stream_id: String,
    pub files: usize,
    pub bytes: u64,
}

impl SnapshotInfo {
    pub fn is_empty(&self) -> bool {
        self.files == 0
    }
}

/// 回滚结果
#[derive(Debug, Clone, Default, Serialize)]
pub struct RestoreReport {
    pub restored: usize,
    /// 备份文件已不存在（例如被清理过）而跳过的条目
    pub missing: usize,
    pub errors: Vec<String>,
}

pub struct SnapshotStore {
    base: PathBuf,
    store: Arc<Mutex<ChatStore>>,
}

impl SnapshotStore {
    pub fn new(app_data_dir: &Path, store: Arc<Mutex<ChatStore>>) -> Self {
        Self {
            base: app_data_dir.join("snapshots"),
            store,
        }
    }

    fn stream_dir(&self, stream_id: &str) -> PathBuf {
        // stream_id 来自后端生成的 UUID；这里再挡一道，避免任何路径拼接
        let safe: String = stream_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        self.base.join(if safe.is_empty() {
            "unknown".to_string()
        } else {
            safe
        })
    }

    /// 备份一个文件（**必须在改动之前调用**）
    ///
    /// `rel` 是相对工作区根的路径，回滚时靠它拼回原位置。
    pub fn capture(
        &self,
        session_id: &str,
        stream_id: &str,
        root_id: &str,
        rel: &Path,
        abs: &Path,
    ) -> Capture {
        let metadata = match fs::metadata(abs) {
            Ok(metadata) => metadata,
            Err(e) => {
                return Capture::Skipped {
                    reason: format!("无法读取原文件（{}）", e),
                }
            }
        };
        if metadata.is_dir() {
            return Capture::Skipped {
                reason: "目录不做内容快照（删除类操作有回收站兜底）".to_string(),
            };
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Capture::Skipped {
                reason: format!(
                    "文件过大（{} KB > {} KB 上限）",
                    metadata.len() / 1024,
                    MAX_FILE_BYTES / 1024
                ),
            };
        }

        let dir = self.stream_dir(stream_id);
        // 非 UTF-8 路径直接拒绝备份：rel_path 要落库为 TEXT，lossy 转换后
        // 根本拼不回原路径（回滚会写到乱码名的新文件上），不如如实告知
        let rel_str = match rel.to_str() {
            Some(rel) => rel.to_string(),
            None => {
                return Capture::Skipped {
                    reason: "路径包含非 UTF-8 字符，无法可靠记录与回滚（已跳过备份）"
                        .to_string(),
                }
            }
        };
        {
            // 只在"查重 + 额度检查"期间持锁：随后的复制最多 4 MB，
            // 让全局 ChatStore 锁陪跑磁盘 IO 会拖住所有消息落库/记忆读写
            let store = match self.store.lock() {
                Ok(store) => store,
                Err(poisoned) => poisoned.into_inner(),
            };

            // 同一个文件在本次生成里只需要备份**最早**的那一份：
            // 回滚的目标是"生成开始前的状态"，而后续备份记录的是中间版本。
            // 不去重的话，回滚按时间顺序重放会把文件停在中间版本；
            // 重复备份也纯属浪费磁盘（每个 stream 有 64 MB 上限）。
            let rows = match store.list_snapshots(stream_id) {
                Ok(rows) => rows,
                Err(e) => {
                    return Capture::Skipped {
                        reason: format!("读取已有备份失败：{}", e),
                    }
                }
            };
            let already_backed_up = rows.iter().any(|row| {
                row.root_id == root_id
                    && row.rel_path == rel_str
                    // 备份文件已被外部清理时不算数，否则会留下"看似可回滚、实际 missing"的记录
                    && dir.join(&row.backup_name).is_file()
            });
            if already_backed_up {
                return Capture::Captured { bytes: 0 };
            }

            let used: u64 = rows.iter().map(|row| row.bytes.max(0) as u64).sum();
            if used + metadata.len() > MAX_TOTAL_BYTES {
                return Capture::Skipped {
                    reason: format!(
                        "本轮备份总量已达上限（{} MB）",
                        MAX_TOTAL_BYTES / 1024 / 1024
                    ),
                };
            }
        }

        if let Err(e) = fs::create_dir_all(&dir) {
            return Capture::Skipped {
                reason: format!("创建备份目录失败：{}", e),
            };
        }
        let backup_name = format!("{}-{}", Uuid::new_v4(), sanitize_name(abs));
        let backup_path = dir.join(&backup_name);
        let copied = match fs::copy(abs, &backup_path) {
            Ok(copied) => copied,
            Err(e) => {
                return Capture::Skipped {
                    reason: format!("复制备份失败：{}", e),
                }
            }
        };
        // 检查与复制之间文件可能已被其它进程写大：以**实际复制的字节数**
        // 为准（既如实记录占盘量，也守住单文件上限）
        if copied > MAX_FILE_BYTES {
            let _ = fs::remove_file(&backup_path);
            return Capture::Skipped {
                reason: format!(
                    "文件在备份期间变大（{} KB > {} KB 上限），已放弃本次备份",
                    copied / 1024,
                    MAX_FILE_BYTES / 1024
                ),
            };
        }

        let row = SnapshotRow {
            id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            stream_id: stream_id.to_string(),
            root_id: root_id.to_string(),
            rel_path: rel_str,
            backup_name,
            bytes: copied as i64,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let store = match self.store.lock() {
            Ok(store) => store,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Err(e) = store.record_snapshot(&row) {
            // 索引写不进去 → 删掉刚复制的备份，避免留下无法回滚的孤儿文件
            let _ = fs::remove_file(&backup_path);
            return Capture::Skipped {
                reason: format!("记录备份索引失败：{}", e),
            };
        }

        Capture::Captured { bytes: copied }
    }

    /// 某个 stream 的快照概况
    pub fn info(&self, session_id: &str, stream_id: &str) -> SnapshotInfo {
        let rows = match self.store.lock() {
            Ok(store) => store.list_snapshots(stream_id).unwrap_or_default(),
            Err(poisoned) => poisoned
                .into_inner()
                .list_snapshots(stream_id)
                .unwrap_or_default(),
        };
        // `files` 是"可回滚的文件数"：同一路径的重复备份只算一个文件
        // （回滚只会应用最早的一份，重复计数会显示成"改动了 2 个文件"而实际只有 1 个）
        let mut seen: HashSet<(&str, &str)> = HashSet::new();
        let files = rows
            .iter()
            .filter(|row| seen.insert((row.root_id.as_str(), row.rel_path.as_str())))
            .count();
        SnapshotInfo {
            session_id: session_id.to_string(),
            stream_id: stream_id.to_string(),
            files,
            bytes: rows.iter().map(|row| row.bytes.max(0) as u64).sum(),
        }
    }

    /// 回滚某个 stream 的全部文件改动
    ///
    /// 目标路径重新过一遍 [`WorkspaceSet::resolve_writable`]：备份是后端自己写的，
    /// 但仍然让它回到同一个信任边界里（这也顺带挡住了 deny_glob 命中的文件与
    /// 只读工作区）。`expected_session` 必须与备份记录的会话一致——否则任何窗口
    /// 拿一个旧 `stream_id` 就能回滚别的会话造成的改动。
    pub fn restore(
        &self,
        stream_id: &str,
        expected_session: &str,
        workspaces: &WorkspaceSet,
    ) -> RestoreReport {
        let rows = match self.store.lock() {
            Ok(store) => store.list_snapshots(stream_id),
            Err(poisoned) => poisoned.into_inner().list_snapshots(stream_id),
        };
        let rows = match rows {
            Ok(rows) => rows,
            Err(e) => {
                return RestoreReport {
                    restored: 0,
                    missing: 0,
                    errors: vec![format!("读取备份索引失败：{}", e)],
                }
            }
        };

        let mut report = RestoreReport::default();
        // 同一路径只还原**最早**的一份备份：`list_snapshots` 按 created_at ASC，
        // 首次出现即生成开始前的原始内容。旧版本可能留下同路径的重复记录
        // （当时会在每次改动前都备份一次），这里做防御性去重——按时间顺序重放
        // 重复记录会把文件停在中间版本，原始内容再也取不回。
        let mut restored_paths: HashSet<(String, String)> = HashSet::new();
        for row in rows {
            let key = (row.root_id.clone(), row.rel_path.clone());
            if !restored_paths.insert(key) {
                continue;
            }
            if row.session_id != expected_session {
                report
                    .errors
                    .push(format!("{}：备份不属于当前会话，已跳过", row.rel_path));
                continue;
            }
            let backup_path = self.stream_dir(stream_id).join(&row.backup_name);
            if !backup_path.is_file() {
                report.missing += 1;
                continue;
            }
            // 用 `root_id:rel_path` 重新走寻址与监狱检查（含只读工作区拒绝）
            let address = format!("{}:{}", row.root_id, row.rel_path);
            let resolved = match workspaces.resolve_writable(&address) {
                Ok(resolved) => resolved,
                Err(e) => {
                    report.errors.push(format!("{}：{}", address, e));
                    continue;
                }
            };
            if let Some(parent) = resolved.abs_path.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    report
                        .errors
                        .push(format!("创建目录失败 {}：{}", parent.display(), e));
                    continue;
                }
            }
            match fs::copy(&backup_path, &resolved.abs_path) {
                Ok(_) => report.restored += 1,
                Err(e) => report
                    .errors
                    .push(format!("还原 {} 失败：{}", resolved.abs_path.display(), e)),
            }
        }
        report
    }

    /// 删除若干 stream 的备份目录（索引行由数据库负责删除）
    ///
    /// 会话被删除时调用：`workspace_snapshots` 行会随会话级联删除，
    /// 但磁盘目录不会；不清理的话这些文件永远回收不了（`prune` 依赖表里的
    /// stream_id 反查目录）。
    pub fn forget_streams(&self, stream_ids: &[String]) -> usize {
        let mut removed = 0;
        for stream in stream_ids {
            let dir = self.stream_dir(stream);
            match fs::remove_dir_all(&dir) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("[snapshot] 删除备份目录失败 {}：{}", dir.display(), e),
            }
        }
        removed
    }

    /// 清理过期备份（索引 + 磁盘文件）
    ///
    /// 返回被清理的 stream 数量。启动时调用一次即可。
    pub fn prune(&self, keep_days: i64) -> usize {
        let expired = match self.store.lock() {
            Ok(store) => store.prune_snapshots(keep_days),
            Err(poisoned) => poisoned.into_inner().prune_snapshots(keep_days),
        };
        let streams = match expired {
            Ok(streams) => streams,
            Err(e) => {
                eprintln!("[snapshot] 清理过期备份失败：{}", e);
                return 0;
            }
        };
        for stream in &streams {
            let dir = self.stream_dir(stream);
            if let Err(e) = fs::remove_dir_all(&dir) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("[snapshot] 删除备份目录失败 {}：{}", dir.display(), e);
                }
            }
        }
        streams.len()
    }
}

/// 备份文件名里只保留安全字符（原文件名仅用于人肉辨认）
fn sanitize_name(path: &Path) -> String {
    let raw = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.chars().take(64).collect()
}

/// 把"备份情况"翻译成给模型和用户看的一句话
///
/// 未做快照时必须**如实说明**：静默地假装备份成功会让用户误以为可以回滚。
pub fn snapshot_note(capture: Option<&Capture>) -> String {
    match capture {
        Some(Capture::Captured { .. }) => {
            "\n（已在改动前备份原文件，界面上可以一键回滚本次改动）".to_string()
        }
        Some(Capture::Skipped { reason }) => format!("\n（未做内容快照：{}）", reason),
        None => "\n（当前环境未启用文件快照，本次改动不可回滚）".to_string(),
    }
}

/// 便捷入口：工具在改动前调用
///
/// 没有快照服务（测试/无存储环境）时返回 `None`，表示"这次改动不可回滚"，
/// 调用方应把这一点写进结果里。
pub fn capture_before_change(
    services: &super::traits::ToolServices,
    session_id: &str,
    stream_id: &str,
    root_id: &str,
    rel: &Path,
    abs: &Path,
) -> Option<Capture> {
    let store = services.snapshots.as_ref()?;
    Some(store.capture(session_id, stream_id, root_id, rel, abs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{ToolConfig, ToolMode};
    use crate::store::db;

    struct Fixture {
        dir: PathBuf,
        snapshots: SnapshotStore,
        workspaces: WorkspaceSet,
        stream_id: String,
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
                .join(format!("konata-snap-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let conn = db::init_db(&dir).unwrap();
            let store = Arc::new(Mutex::new(ChatStore::new(conn)));
            let session = store
                .lock()
                .unwrap()
                .create_session("konata-default", "测试", None, None, None)
                .unwrap();
            let cfg = ToolConfig::with_single_root(&dir, true, "测试");
            let workspaces = WorkspaceSet::from_config(&cfg, &dir);
            let snapshots = SnapshotStore::new(&dir, store);
            Self {
                dir,
                snapshots,
                workspaces,
                stream_id: "stream-1".to_string(),
                session_id: session.id,
            }
        }

        fn rel(&self, name: &str) -> (PathBuf, PathBuf) {
            (PathBuf::from(name), self.dir.join(name))
        }
    }

    #[test]
    fn capture_then_restore_returns_original_content() {
        let fx = Fixture::new("restore");
        let (rel, abs) = fx.rel("note.txt");
        std::fs::write(&abs, "原始内容").unwrap();

        let captured = fx
            .snapshots
            .capture(&fx.session_id, &fx.stream_id, "default", &rel, &abs);
        assert!(captured.is_captured(), "{captured:?}");

        // 模拟一次破坏性改动
        std::fs::write(&abs, "被改坏了").unwrap();
        let info = fx.snapshots.info(&fx.session_id, &fx.stream_id);
        assert_eq!(info.files, 1);
        assert!(info.bytes > 0);

        let report = fx.snapshots.restore(&fx.stream_id, &fx.session_id, &fx.workspaces);
        assert_eq!(report.restored, 1, "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(std::fs::read_to_string(&abs).unwrap(), "原始内容");
    }

    /// 同一文件连续两次改动：只保留最早一份备份，回滚必须回到生成开始前
    ///
    /// 修复前：每次改动前都备份，回滚按时间顺序重放 → 最终停在中间版本，
    /// 原始内容再也取不回（`write_file` 后再 `edit_file` 的常见模式）。
    #[test]
    fn repeated_capture_keeps_the_earliest_backup() {
        let fx = Fixture::new("repeat");
        let (rel, abs) = fx.rel("note.txt");
        std::fs::write(&abs, "v0").unwrap();

        let first = fx
            .snapshots
            .capture(&fx.session_id, &fx.stream_id, "default", &rel, &abs);
        assert!(first.is_captured(), "{first:?}");

        std::fs::write(&abs, "v1").unwrap();
        let second = fx
            .snapshots
            .capture(&fx.session_id, &fx.stream_id, "default", &rel, &abs);
        assert!(second.is_captured(), "已有备份时仍应报告可回滚：{second:?}");

        std::fs::write(&abs, "v2").unwrap();
        let info = fx.snapshots.info(&fx.session_id, &fx.stream_id);
        assert_eq!(info.files, 1, "同一路径不能被计成两个文件");

        let report = fx.snapshots.restore(&fx.stream_id, &fx.session_id, &fx.workspaces);
        assert_eq!(report.restored, 1, "{report:?}");
        assert_eq!(std::fs::read_to_string(&abs).unwrap(), "v0");
    }

    /// 旧版本留下的重复记录：回滚必须挑最早一条，而不是按时间顺序重放
    #[test]
    fn restore_of_legacy_duplicate_rows_prefers_the_earliest() {
        let fx = Fixture::new("earliest");
        let (rel, abs) = fx.rel("legacy.txt");
        std::fs::write(&abs, "中间版本").unwrap();

        let dir = fx.snapshots.stream_dir(&fx.stream_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("first.bak"), "原始版本").unwrap();
        std::fs::write(dir.join("second.bak"), "中间版本").unwrap();
        {
            let store = fx.snapshots.store.lock().unwrap();
            for (id, backup, at) in [
                ("r1", "first.bak", "2026-01-01T00:00:00+00:00"),
                ("r2", "second.bak", "2026-01-01T00:00:01+00:00"),
            ] {
                store
                    .record_snapshot(&SnapshotRow {
                        id: id.to_string(),
                        session_id: fx.session_id.clone(),
                        stream_id: fx.stream_id.clone(),
                        root_id: "default".to_string(),
                        rel_path: rel.to_string_lossy().to_string(),
                        backup_name: backup.to_string(),
                        bytes: 12,
                        created_at: at.to_string(),
                    })
                    .unwrap();
            }
        }

        let report = fx.snapshots.restore(&fx.stream_id, &fx.session_id, &fx.workspaces);
        assert_eq!(report.restored, 1, "{report:?}");
        assert_eq!(std::fs::read_to_string(&abs).unwrap(), "原始版本");
    }

    /// 非 UTF-8 路径无法可靠落库/回滚，必须跳过并如实说明
    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_skipped_with_reason() {
        use std::os::unix::ffi::OsStrExt;

        let fx = Fixture::new("nonutf8");
        let name = std::ffi::OsStr::from_bytes(b"bad-\xFF.txt");
        let abs = fx.dir.join(name);
        std::fs::write(&abs, "内容").unwrap();
        let rel = PathBuf::from(name);

        match fx
            .snapshots
            .capture(&fx.session_id, &fx.stream_id, "default", &rel, &abs)
        {
            Capture::Skipped { reason } => {
                assert!(reason.contains("UTF-8"), "{reason}");
            }
            other => panic!("非 UTF-8 路径必须跳过备份：{other:?}"),
        }
        assert_eq!(fx.snapshots.info(&fx.session_id, &fx.stream_id).files, 0);
    }

    #[test]
    fn restore_recreates_deleted_files() {
        let fx = Fixture::new("deleted");
        let (rel, abs) = fx.rel("gone/path.txt");
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, "要找回我").unwrap();
        fx.snapshots
            .capture(&fx.session_id, &fx.stream_id, "default", &rel, &abs);

        std::fs::remove_file(&abs).unwrap();
        assert!(!abs.exists());

        let report = fx.snapshots.restore(&fx.stream_id, &fx.session_id, &fx.workspaces);
        assert_eq!(report.restored, 1, "{report:?}");
        assert_eq!(std::fs::read_to_string(&abs).unwrap(), "要找回我");
    }

    #[test]
    fn oversized_files_are_skipped_with_reason() {
        let fx = Fixture::new("huge");
        let (rel, abs) = fx.rel("big.bin");
        let file = std::fs::File::create(&abs).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(file);

        let captured = fx
            .snapshots
            .capture(&fx.session_id, &fx.stream_id, "default", &rel, &abs);
        match captured {
            Capture::Skipped { reason } => assert!(reason.contains("过大"), "{reason}"),
            other => panic!("大文件必须跳过备份：{other:?}"),
        }
        assert_eq!(fx.snapshots.info(&fx.session_id, &fx.stream_id).files, 0);
    }

    #[test]
    fn directories_are_not_captured() {
        let fx = Fixture::new("dir");
        let (rel, abs) = fx.rel("sub");
        std::fs::create_dir_all(&abs).unwrap();
        let captured = fx
            .snapshots
            .capture(&fx.session_id, &fx.stream_id, "default", &rel, &abs);
        match captured {
            Capture::Skipped { reason } => assert!(reason.contains("目录"), "{reason}"),
            other => panic!("目录不该做内容快照：{other:?}"),
        }
    }

    #[test]
    fn missing_backup_is_reported_not_swallowed() {
        let fx = Fixture::new("missing");
        let (rel, abs) = fx.rel("a.txt");
        std::fs::write(&abs, "x").unwrap();
        fx.snapshots
            .capture(&fx.session_id, &fx.stream_id, "default", &rel, &abs);

        // 模拟备份文件被外部清理
        let backup_dir = fx.snapshots.stream_dir(&fx.stream_id);
        for entry in std::fs::read_dir(&backup_dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let report = fx.snapshots.restore(&fx.stream_id, &fx.session_id, &fx.workspaces);
        assert_eq!(report.restored, 0);
        assert_eq!(report.missing, 1, "{report:?}");
    }

    #[test]
    fn prune_removes_expired_backups() {
        let fx = Fixture::new("prune");
        let (rel, abs) = fx.rel("old.txt");
        std::fs::write(&abs, "旧").unwrap();
        fx.snapshots
            .capture(&fx.session_id, &fx.stream_id, "default", &rel, &abs);

        // 保留 0 天 = 立刻过期
        let pruned = fx.snapshots.prune(0);
        assert_eq!(pruned, 1);
        assert_eq!(fx.snapshots.info(&fx.session_id, &fx.stream_id).files, 0);
        assert!(
            !fx.snapshots.stream_dir(&fx.stream_id).exists(),
            "过期后备份目录也应当被清掉"
        );
    }

    #[test]
    fn stream_id_cannot_escape_the_snapshot_root() {
        let fx = Fixture::new("escape");
        let dir = fx.snapshots.stream_dir("../../etc");
        assert!(
            dir.starts_with(&fx.snapshots.base),
            "非法 stream id 不能跳出备份根目录：{}",
            dir.display()
        );
    }

    #[test]
    fn mode_is_irrelevant_to_snapshots() {
        // 快照属于 harness 服务层，不随工具模式变化（只为可回滚性负责）
        let fx = Fixture::new("mode");
        let cfg = ToolConfig::with_single_root(&fx.dir, true, "测试");
        let set = WorkspaceSet::from_config(&cfg, &fx.dir);
        assert!(set.resolve("note.txt").is_ok());
        let _ = ToolMode::Standard;
    }
}
