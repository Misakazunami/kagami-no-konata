//! bundle id 分家后的一次性数据目录迁移
//!
//! # 背景（真实事故）
//!
//! 本应用曾与另一条开发线使用**同一个 bundle id** `com.konata-mirror.app`，
//! 于是两套构建共用同一个 `data.db`。两边的迁移编号还互相冲突
//! （本仓库 `005` = `005_perf`，那条线 `005` = `005_attachments`），
//! 结果对方把版本号推到 16 之后，本仓库的迁移全部被跳过，
//! `sessions.context_summary` 永远没被创建 —— 应用启动正常，
//! 用户发第一条消息才炸 `no such column: context_summary`。
//!
//! 分家的做法是把本仓库的 identifier 改成独立值。但用户此刻的聊天记录、
//! API Key、人格与工作区文件都躺在**旧目录**里，一旦换了 identifier，
//! 新目录是空的，用户会以为"数据全丢了"。
//!
//! 因此这里做一次**只复制、不删除**的迁移：
//! - 仅当新目录是全新的（没有 `data.db` 也没有 `config.json`）才执行；
//! - 旧目录原样保留（它仍是那条开发线的数据，也是天然的备份）；
//! - 留下 `.legacy-data-migrated` 标记，避免用户主动清空数据后被"复活"。

use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};

/// 旧的（共享的）bundle id 目录名
pub const LEGACY_IDENTIFIER: &str = "com.konata-mirror.app";

/// 迁移完成标记
const MARKER: &str = ".legacy-data-migrated";

/// 工作区目录的复制上限：超过就只提示、不复制，避免拖慢启动
const MAX_WORKSPACE_COPY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// 是否真的复制了东西
    pub performed: bool,
    /// 已复制项的名称（给人看的）
    pub copied: Vec<String>,
    /// 未执行的原因（已执行时为 None）
    pub skipped_reason: Option<String>,
    /// 需要用户手动处理的事项
    pub notes: Vec<String>,
}

impl MigrationReport {
    fn skipped(reason: impl Into<String>) -> Self {
        Self {
            performed: false,
            copied: Vec::new(),
            skipped_reason: Some(reason.into()),
            notes: Vec::new(),
        }
    }
}

/// 推断旧数据目录：当前数据目录的**同级兄弟**目录 `com.konata-mirror.app`
///
/// 当前目录本身就是旧目录（还没改 identifier）时返回 `None`。
pub fn resolve_legacy_dir(current_dir: &Path) -> Option<PathBuf> {
    let name = current_dir.file_name()?.to_string_lossy().to_string();
    if name == LEGACY_IDENTIFIER {
        return None;
    }
    let parent = current_dir.parent()?;
    let legacy = parent.join(LEGACY_IDENTIFIER);
    legacy.is_dir().then_some(legacy)
}

/// 执行一次性迁移（幂等）
pub fn migrate_legacy_data_dir(current_dir: &Path, legacy_dir: &Path) -> Result<MigrationReport> {
    if current_dir.join(MARKER).exists() {
        return Ok(MigrationReport::skipped("此前已迁移过"));
    }
    // 新目录一旦有真实数据就不再动手，避免覆盖用户已经产生的内容
    if current_dir.join("data.db").exists() || current_dir.join("config.json").exists() {
        return Ok(MigrationReport::skipped(
            "新数据目录已有数据，不做自动迁移",
        ));
    }
    if !legacy_dir.is_dir() {
        return Ok(MigrationReport::skipped("旧数据目录不存在"));
    }

    std::fs::create_dir_all(current_dir)?;
    let mut report = MigrationReport {
        performed: true,
        ..Default::default()
    };

    // ─── 1. 数据库：优先用 VACUUM INTO 做一致快照 ───
    // WAL 模式下直接复制 data.db 可能拿到缺最新事务的中间态，
    // VACUUM INTO 由 SQLite 自己保证一致性（且需要目标文件不存在）。
    let legacy_db = legacy_dir.join("data.db");
    if legacy_db.is_file() {
        let target_db = current_dir.join("data.db");
        if copy_sqlite_snapshot(&legacy_db, &target_db) {
            report.copied.push("data.db（一致快照）".to_string());
        } else {
            // 退化路径：db + wal + shm 三个文件一起复制，
            // SQLite 打开时会自行做 WAL 恢复
            let _ = std::fs::remove_file(&target_db);
            let mut any = false;
            for name in ["data.db", "data.db-wal", "data.db-shm"] {
                let src = legacy_dir.join(name);
                if src.is_file() {
                    std::fs::copy(&src, current_dir.join(name))?;
                    any = true;
                }
            }
            if any {
                report.copied.push("data.db（原始文件）".to_string());
                report
                    .notes
                    .push("数据库为直接复制，已由 SQLite 在打开时做 WAL 恢复".to_string());
            } else {
                report.notes.push("旧目录没有可用的 data.db".to_string());
            }
        }
    }

    // ─── 2. 配置（含 API Key 与工具设置）───
    let legacy_config = legacy_dir.join("config.json");
    if legacy_config.is_file() {
        std::fs::copy(&legacy_config, current_dir.join("config.json"))?;
        report.copied.push("config.json".to_string());
    }

    // ─── 3. 人格 ───
    let legacy_personas = legacy_dir.join("personas");
    if legacy_personas.is_dir() {
        let copied = copy_dir_recursive(&legacy_personas, &current_dir.join("personas"))?;
        report
            .copied
            .push(format!("personas/（{} 个文件）", copied));
    }

    // ─── 4. 工作区（工具文件沙箱）───
    let legacy_workspace = legacy_dir.join("workspace");
    if legacy_workspace.is_dir() {
        match dir_size(&legacy_workspace) {
            Ok(size) if size <= MAX_WORKSPACE_COPY_BYTES => {
                let copied = copy_dir_recursive(&legacy_workspace, &current_dir.join("workspace"))?;
                report
                    .copied
                    .push(format!("workspace/（{} 个文件）", copied));
            }
            Ok(size) => report.notes.push(format!(
                "旧工作区有 {:.1} MB，超过自动复制上限，未复制；如需要请手动拷贝 {}",
                size as f64 / 1024.0 / 1024.0,
                legacy_workspace.display()
            )),
            Err(e) => report
                .notes
                .push(format!("无法统计旧工作区大小，未复制：{}", e)),
        }
    }

    // ─── 5. 标记 ───
    std::fs::write(
        current_dir.join(MARKER),
        format!(
            "from={}\nat={}\nitems={}\n",
            legacy_dir.display(),
            Utc::now().to_rfc3339(),
            report.copied.join(", ")
        ),
    )?;

    Ok(report)
}

/// 用 `VACUUM INTO` 生成一致快照；目标已存在或失败时返回 false
fn copy_sqlite_snapshot(src: &Path, dst: &Path) -> bool {
    // VACUUM INTO 要求目标文件不存在
    let _ = std::fs::remove_file(dst);
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        src,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    ) else {
        return false;
    };
    conn.execute("VACUUM INTO ?1", [dst.to_string_lossy().to_string()])
        .is_ok()
}

/// 递归复制目录，返回复制的文件数
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<usize> {
    let mut count = 0usize;
    std::fs::create_dir_all(dst)?;
    for entry in walkdir::WalkDir::new(src).min_depth(1) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let relative = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dst.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
            count += 1;
        }
    }
    Ok(count)
}

fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{chat_store::ChatStore, db};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("konata-migrate-{}-{}", tag, uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 造一份"旧数据目录"：真实数据库 + 一条会话 + 配置 + 人格 + 工作区文件
    fn seed_legacy(dir: &Path) {
        let conn = db::init_db(dir).unwrap();
        let store = ChatStore::new(conn);
        let session = store.create_session("konata-default", "旧会话", None, None, None).unwrap();
        store
            .add_message(&session.id, crate::agent::context::Role::User, "你好", 1, 0, None, None)
            .unwrap();

        std::fs::write(dir.join("config.json"), "{\"user\":{\"nickname\":\"主人\"}}").unwrap();
        std::fs::create_dir_all(dir.join("personas")).unwrap();
        std::fs::write(dir.join("personas/mine.yaml"), "id: mine\nname: 我的角色\nsystem_prompt: x\n")
            .unwrap();
        std::fs::create_dir_all(dir.join("workspace/notes")).unwrap();
        std::fs::write(dir.join("workspace/notes/a.md"), "笔记").unwrap();
    }

    #[test]
    fn migrates_database_config_personas_and_workspace() {
        let root = TempDir::new("full");
        let legacy = root.path().join(LEGACY_IDENTIFIER);
        let current = root.path().join("com.konata-mirror.main");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        seed_legacy(&legacy);

        let report = migrate_legacy_data_dir(&current, &legacy).unwrap();
        assert!(report.performed, "{:?}", report);
        assert!(report.skipped_reason.is_none());

        // 会话必须跟着过来
        let conn = db::init_db(&current).unwrap();
        let store = ChatStore::new(conn);
        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "旧会话");
        assert_eq!(store.get_messages(&sessions[0].id).unwrap().len(), 1);

        assert!(current.join("config.json").exists());
        assert!(current.join("personas/mine.yaml").exists());
        assert_eq!(
            std::fs::read_to_string(current.join("workspace/notes/a.md")).unwrap(),
            "笔记"
        );
        assert!(current.join(MARKER).exists());

        // 旧目录必须原样保留（既是那条线的数据，也是备份）
        assert!(legacy.join("data.db").exists());
        assert!(legacy.join("config.json").exists());
        assert!(legacy.join("personas/mine.yaml").exists());
    }

    #[test]
    fn refuses_to_touch_a_directory_that_already_has_data() {
        let root = TempDir::new("occupied");
        let legacy = root.path().join(LEGACY_IDENTIFIER);
        let current = root.path().join("com.konata-mirror.main");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        seed_legacy(&legacy);
        // 新目录已经有自己的数据
        std::fs::write(current.join("config.json"), "{\"user\":{\"nickname\":\"新\"}}").unwrap();

        let report = migrate_legacy_data_dir(&current, &legacy).unwrap();
        assert!(!report.performed);
        assert!(report.skipped_reason.is_some());
        // 既有数据不得被覆盖
        assert!(std::fs::read_to_string(current.join("config.json"))
            .unwrap()
            .contains("新"));
    }

    #[test]
    fn marker_prevents_resurrection_after_user_clears_data() {
        let root = TempDir::new("marker");
        let legacy = root.path().join(LEGACY_IDENTIFIER);
        let current = root.path().join("com.konata-mirror.main");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        seed_legacy(&legacy);

        assert!(migrate_legacy_data_dir(&current, &legacy).unwrap().performed);

        // 用户主动清空新目录里的数据
        std::fs::remove_file(current.join("data.db")).unwrap();
        std::fs::remove_file(current.join("config.json")).unwrap();

        let second = migrate_legacy_data_dir(&current, &legacy).unwrap();
        assert!(!second.performed, "用户清空后不得被自动复活");
        assert!(!current.join("data.db").exists());
    }

    #[test]
    fn missing_legacy_dir_is_a_no_op() {
        let root = TempDir::new("absent");
        let current = root.path().join("com.konata-mirror.main");
        std::fs::create_dir_all(&current).unwrap();

        let report =
            migrate_legacy_data_dir(&current, &root.path().join(LEGACY_IDENTIFIER)).unwrap();
        assert!(!report.performed);
    }

    #[test]
    fn resolve_legacy_dir_only_when_sibling_exists() {
        let root = TempDir::new("resolve");
        let current = root.path().join("com.konata-mirror.main");
        std::fs::create_dir_all(&current).unwrap();

        // 兄弟目录不存在 → None
        assert!(resolve_legacy_dir(&current).is_none());

        std::fs::create_dir_all(root.path().join(LEGACY_IDENTIFIER)).unwrap();
        let found = resolve_legacy_dir(&current).unwrap();
        assert!(found.ends_with(LEGACY_IDENTIFIER));

        // 自己就是旧目录 → None（没改 identifier 时不该自我迁移）
        let legacy_self = root.path().join(LEGACY_IDENTIFIER);
        assert!(resolve_legacy_dir(&legacy_self).is_none());
    }

    #[test]
    fn snapshot_copy_is_consistent_even_with_wal_present() {
        let root = TempDir::new("wal");
        let legacy = root.path().join(LEGACY_IDENTIFIER);
        let current = root.path().join("com.konata-mirror.main");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&current).unwrap();

        // 写入后不 checkpoint，制造"数据还在 WAL 里"的状态
        let conn = db::init_db(&legacy).unwrap();
        let store = ChatStore::new(conn);
        let session = store.create_session("konata-default", "WAL 里的会话", None, None, None).unwrap();
        store
            .add_message(&session.id, crate::agent::context::Role::User, "hi", 1, 0, None, None)
            .unwrap();
        assert!(legacy.join("data.db-wal").exists(), "应处于 WAL 模式");
        drop(store);

        migrate_legacy_data_dir(&current, &legacy).unwrap();

        let conn = db::init_db(&current).unwrap();
        let store = ChatStore::new(conn);
        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1, "WAL 中的事务也必须出现在快照里");
        assert_eq!(sessions[0].title, "WAL 里的会话");
    }
}
