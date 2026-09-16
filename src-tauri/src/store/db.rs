use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// 迁移脚本列表（版本号必须严格递增且与 `migrations/` 下的文件名一致）
const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("migrations/001_init.sql")),
    (2, include_str!("migrations/002_memory.sql")),
    (3, include_str!("migrations/003_stats.sql")),
    (4, include_str!("migrations/004_thinking.sql")),
    (5, include_str!("migrations/005_perf.sql")),
    (6, include_str!("migrations/006_tools.sql")),
    (7, include_str!("migrations/007_plan.sql")),
    (8, include_str!("migrations/008_snapshots.sql")),
    (9, include_str!("migrations/009_notes.sql")),
    (10, include_str!("migrations/010_session_types.sql")),
    (11, include_str!("migrations/011_session_model_pref.sql")),
];

/// 打开数据库连接（应用 WAL 等 PRAGMA，不执行迁移）
///
/// `foreign_keys` 等 PRAGMA 是连接级开关，每个连接都需要独立设置。
pub fn open_connection(app_data_dir: &Path) -> Result<Connection> {
    std::fs::create_dir_all(app_data_dir)?;
    let db_path = app_data_dir.join("data.db");
    let conn = Connection::open(db_path)?;

    // 启用 WAL 模式 + 并发写入保护
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;
         PRAGMA foreign_keys=ON;",
    )?;

    Ok(conn)
}

/// 建表 / 建索引语句（全部带 `IF NOT EXISTS`，可安全重复执行）
///
/// 单独抽出来是因为它们必须**绕过版本号**每次启动都跑一遍：见 [`ensure_schema`]。
const CREATE_SCRIPTS: &[&str] = &[
    include_str!("migrations/001_init.sql"),
    include_str!("migrations/002_memory.sql"),
    include_str!("migrations/006_tools.sql"),
    include_str!("migrations/007_plan.sql"),
    include_str!("migrations/008_snapshots.sql"),
    include_str!("migrations/009_notes.sql"),
    STATS_DDL,
];

/// 003 把建表与 `ALTER TABLE` 混在同一个文件里，无法整体重复执行，
/// 因此建表部分在这里重述（`schema_fingerprint` 一致性测试会守住两边不漂移）。
const STATS_DDL: &str = "
CREATE TABLE IF NOT EXISTS usage_stats (
    id              INTEGER PRIMARY KEY DEFAULT 1,
    total_requests  INTEGER NOT NULL DEFAULT 0,
    total_tokens    INTEGER NOT NULL DEFAULT 0,
    prompt_tokens   INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_time_ms   INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT OR IGNORE INTO usage_stats (id) VALUES (1);
";

/// 期望存在的列（`ALTER TABLE ADD COLUMN` 无法写 `IF NOT EXISTS`，只能先探测再补）
///
/// 元组含义：`(表, 列, 补列 DDL)`
const REQUIRED_COLUMNS: &[(&str, &str, &str)] = &[
    // 003_stats
    (
        "messages",
        "token_count",
        "ALTER TABLE messages ADD COLUMN token_count INTEGER DEFAULT 0",
    ),
    (
        "messages",
        "thinking_ms",
        "ALTER TABLE messages ADD COLUMN thinking_ms INTEGER DEFAULT 0",
    ),
    // 004_thinking
    (
        "messages",
        "thinking",
        "ALTER TABLE messages ADD COLUMN thinking TEXT DEFAULT NULL",
    ),
    // 005_perf
    (
        "sessions",
        "context_summary",
        "ALTER TABLE sessions ADD COLUMN context_summary TEXT DEFAULT NULL",
    ),
    (
        "sessions",
        "summarized_count",
        "ALTER TABLE sessions ADD COLUMN summarized_count INTEGER NOT NULL DEFAULT 0",
    ),
    (
        "sessions",
        "last_extracted_at",
        "ALTER TABLE sessions ADD COLUMN last_extracted_at TEXT DEFAULT NULL",
    ),
    (
        "memories",
        "embedding_blob",
        "ALTER TABLE memories ADD COLUMN embedding_blob BLOB DEFAULT NULL",
    ),
    // 010_session_types
    (
        "sessions",
        "session_type",
        "ALTER TABLE sessions ADD COLUMN session_type TEXT NOT NULL DEFAULT 'chat'",
    ),
    (
        "sessions",
        "task_mode",
        "ALTER TABLE sessions ADD COLUMN task_mode TEXT NOT NULL DEFAULT 'plan'",
    ),
    (
        "sessions",
        "workspace_id",
        "ALTER TABLE sessions ADD COLUMN workspace_id TEXT DEFAULT NULL",
    ),
    // 011_session_model_pref
    (
        "sessions",
        "model_pref",
        "ALTER TABLE sessions ADD COLUMN model_pref TEXT DEFAULT NULL",
    ),
];

/// 初始化数据库连接并执行迁移
///
/// 迁移在进程内只需执行一次；后续连接请使用 [`open_connection`]，
/// 避免多个连接重复跑迁移。
pub fn init_db(app_data_dir: &Path) -> Result<Connection> {
    let conn = open_connection(app_data_dir)?;
    run_migrations(&conn)?;
    ensure_schema(&conn)?;
    Ok(conn)
}

/// 幂等的 schema 兜底修复（每次启动都跑）
///
/// **为什么不能只信 `schema_version`**：版本号是一份"别人的账本"。
/// 实测过一次真实事故——用户的 `data.db` 由同一应用的另一条开发线创建，
/// 那份历史的版本号已经记到 16，而本仓库只有 1~6。于是 `run_migrations`
/// 认为"什么都不用做"，`sessions.context_summary`（本仓库的 005）永远不会被创建，
/// 应用启动正常、直到用户发第一条消息时才炸出
/// `no such column: context_summary`。
///
/// 因此这里改用**结构化声明**（需要哪些表、哪些列）+ 幂等补齐：
/// 与版本号无关，任何来源的库补到本仓库需要的形状即可。
/// 全部操作都是 `IF NOT EXISTS` 或"先探测再补列"，重复执行无副作用。
fn ensure_schema(conn: &Connection) -> Result<()> {
    for script in CREATE_SCRIPTS {
        conn.execute_batch(script)?;
    }

    for (table, column, ddl) in REQUIRED_COLUMNS {
        if !column_exists(conn, table, column)? {
            conn.execute_batch(ddl)?;
        }
    }

    Ok(())
}

/// 表是否拥有某列（表不存在时返回 false，交由建表语句处理）
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 执行数据库迁移
fn run_migrations(conn: &Connection) -> Result<()> {
    // 创建迁移版本表
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )?;

    // 获取当前版本
    let current_version: i32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    for (version, sql) in MIGRATIONS {
        if *version <= current_version {
            continue;
        }

        // 迁移 DDL 与版本号写入必须在同一事务内：
        // 否则 DDL 成功而版本号未落库时，重启会重复执行 `ALTER TABLE ADD COLUMN`
        // （SQLite 不支持 ADD COLUMN IF NOT EXISTS）→ 报 duplicate column →
        // init_db 失败 → 应用永久无法启动。
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.execute("INSERT INTO schema_version (version) VALUES (?1)", [version])?;
        tx.commit()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "konata-db-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// 期望的 schema 版本 = 迁移表里最大的版本号（新增迁移不必再改测试）
    fn latest_version() -> i32 {
        MIGRATIONS.iter().map(|(v, _)| *v).max().unwrap_or(0)
    }

    fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({})", table))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    /// 所有表及其列的快照，用于断言幂等性
    fn schema_fingerprint(conn: &Connection) -> Vec<(String, Vec<String>)> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        tables
            .into_iter()
            .map(|table| {
                let columns = table_columns(conn, &table);
                (table, columns)
            })
            .collect()
    }

    fn schema_version(conn: &Connection) -> i32 {
        conn.query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    #[test]
    fn init_db_applies_all_migrations_and_is_idempotent() {
        let dir = temp_dir("init");
        let conn = init_db(&dir).expect("init db");
        assert_eq!(schema_version(&conn), latest_version());

        conn.execute(
            "INSERT INTO sessions (id, title, persona_id, created_at, updated_at) VALUES ('s','t','p','now','now')",
            [],
        )
        .unwrap();
        // 004 / 005 新增的列必须存在
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, created_at, date_key, token_count, thinking_ms, thinking)
             VALUES ('m','s','user','hi','2024-01-01T00:00:00+00:00','2024-01-01',1,2,'think')",
            [],
        )
        .unwrap();
        assert!(conn
            .query_row(
                "SELECT context_summary, summarized_count, last_extracted_at FROM sessions WHERE id='s'",
                [],
                |r| Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                )),
            )
            .is_ok());

        // 二次初始化（模拟重启）必须成功，且不会重复执行 ALTER TABLE
        drop(conn);
        let conn2 = init_db(&dir).expect("re-init db must succeed");
        assert_eq!(schema_version(&conn2), latest_version());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 复现真实事故：`schema_version` 被"别人的开发线"记到 16，
    /// 但本仓库需要的列（sessions.context_summary 等）并不存在。
    ///
    /// 修复前：`run_migrations` 认为无事可做 → 启动正常 → 发消息时炸
    /// `no such column: context_summary`。
    /// 修复后：`ensure_schema` 无视版本号把缺的列补齐。
    #[test]
    fn repairs_schema_when_version_ledger_belongs_to_another_build() {
        let dir = temp_dir("foreign-ledger");
        std::fs::create_dir_all(&dir).unwrap();

        {
            let conn = open_connection(&dir).unwrap();
            // 另一条开发线的表结构：sessions 有自己的额外列，但没有 context_summary
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL DEFAULT '新会话',
                    persona_id TEXT NOT NULL DEFAULT 'konata-default',
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    scene_id TEXT
                );
                CREATE TABLE schema_version (
                    version INTEGER PRIMARY KEY,
                    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
                );",
            )
            .unwrap();
            for version in 1..=16 {
                conn.execute("INSERT INTO schema_version (version) VALUES (?1)", [version])
                    .unwrap();
            }
            // 版本号远超本仓库的 6：修复前这里会让所有迁移被跳过
            assert_eq!(schema_version(&conn), 16);
            assert!(!column_exists(&conn, "sessions", "context_summary").unwrap());
        }

        // 启动 → 必须把缺的列补齐，且不报错
        let conn = init_db(&dir).expect("foreign ledger db must be repaired, not rejected");
        for (table, column) in [
            ("sessions", "context_summary"),
            ("sessions", "summarized_count"),
            ("sessions", "last_extracted_at"),
            ("messages", "token_count"),
            ("messages", "thinking_ms"),
            ("messages", "thinking"),
            ("memories", "embedding_blob"),
        ] {
            assert!(
                column_exists(&conn, table, column).unwrap(),
                "修复后必须存在列 {}.{}",
                table,
                column
            );
        }
        // 另一条开发线留下的列不能被破坏
        assert!(column_exists(&conn, "sessions", "scene_id").unwrap());
        // 新增表也必须建好（本仓库的 006）
        assert!(conn
            .query_row("SELECT COUNT(*) FROM tool_invocations", [], |r| r.get::<_, i64>(0))
            .is_ok());

        // 真正触发过事故的那条查询现在必须能跑
        conn.execute(
            "INSERT INTO sessions (id, title, persona_id, created_at, updated_at)
             VALUES ('s1','t','konata-default','now','now')",
            [],
        )
        .unwrap();
        let (summary, count): (String, i64) = conn
            .query_row(
                "SELECT COALESCE(context_summary, ''), summarized_count FROM sessions WHERE id = ?1",
                ["s1"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("事故查询必须恢复可用");
        assert_eq!(summary, "");
        assert_eq!(count, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 每次启动都会跑 ensure_schema：必须完全幂等
    #[test]
    fn ensure_schema_is_idempotent() {
        let dir = temp_dir("ensure-idempotent");
        let conn = init_db(&dir).unwrap();
        let before = schema_fingerprint(&conn);

        for _ in 0..3 {
            ensure_schema(&conn).expect("重复执行不得报错");
            run_migrations(&conn).expect("重复执行迁移不得报错");
        }

        assert_eq!(schema_fingerprint(&conn), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全新库与"被修复过的外来库"最终形状必须一致（防止声明表漏项）
    #[test]
    fn fresh_and_repaired_schemas_match() {
        let fresh_dir = temp_dir("fresh-shape");
        let fresh = init_db(&fresh_dir).unwrap();

        let foreign_dir = temp_dir("foreign-shape");
        std::fs::create_dir_all(&foreign_dir).unwrap();
        {
            let conn = open_connection(&foreign_dir).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
                 INSERT INTO schema_version (version) VALUES (16);",
            )
            .unwrap();
        }
        let repaired = init_db(&foreign_dir).unwrap();

        for table in [
            "sessions",
            "messages",
            "memories",
            "usage_stats",
            "tool_invocations",
            "session_plans",
            "workspace_snapshots",
            "tool_notes",
        ] {
            let fresh_cols = table_columns(&fresh, table);
            let repaired_cols = table_columns(&repaired, table);
            for column in &fresh_cols {
                assert!(
                    repaired_cols.contains(column),
                    "修复后的 {} 缺少列 {}（fresh={:?} repaired={:?}）",
                    table,
                    column,
                    fresh_cols,
                    repaired_cols
                );
            }
        }

        let _ = std::fs::remove_dir_all(&fresh_dir);
        let _ = std::fs::remove_dir_all(&foreign_dir);
    }

    /// 用手上**真实**的 data.db 副本验证修复（默认忽略，不依赖开发机文件）
    ///
    /// 用法：
    /// `KONATA_REAL_DB=~/.local/share/com.konata-mirror.main/data.db \
    ///   cargo test repairs_a_real_database_copy -- --ignored --nocapture`
    ///
    /// 先用 `VACUUM INTO` 做一致性只读副本（原库不会被改动，即使应用正在运行）。
    #[test]
    #[ignore = "需要 KONATA_REAL_DB 指向真实 data.db"]
    fn repairs_a_real_database_copy() {
        let Some(source) = std::env::var_os("KONATA_REAL_DB") else {
            eprintln!("未设置 KONATA_REAL_DB，跳过");
            return;
        };

        let dir = temp_dir("real-db-copy");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("data.db");

        {
            let src = Connection::open_with_flags(
                &source,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .expect("打开真实库（只读）");
            src.execute("VACUUM INTO ?1", [target.display().to_string()])
                .expect("复制真实库");
        }

        let missing_before = {
            let conn = open_connection(&dir).unwrap();
            let ledger = schema_version(&conn);
            let missing: Vec<&str> = REQUIRED_COLUMNS
                .iter()
                .filter(|(table, column, _)| !column_exists(&conn, table, column).unwrap())
                .map(|(_, column, _)| *column)
                .collect();
            println!("真实库副本：schema_version={}，缺失列={:?}", ledger, missing);
            missing
        };
        assert!(
            !missing_before.is_empty(),
            "这份库本不该缺列；若已修好，说明修复已生效过一次"
        );

        let conn = init_db(&dir).expect("真实库必须被修复而不是被拒绝");

        for (table, column, _) in REQUIRED_COLUMNS {
            assert!(
                column_exists(&conn, table, column).unwrap(),
                "修复后仍缺列 {}.{}",
                table,
                column
            );
        }

        // 事故现场的那条查询 + 一次真实写入（触发外键与 NOT NULL 约束）
        conn.execute(
            "INSERT INTO sessions (id, title, persona_id, created_at, updated_at)
             VALUES ('probe','t','konata-default','now','now')",
            [],
        )
        .expect("会话插入必须成功");
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, created_at, date_key, token_count, thinking_ms, thinking)
             VALUES ('probe-m','probe','user','hi','now','2026-01-01',1,0,NULL)",
            [],
        )
        .expect("消息插入必须成功");
        let (summary, count): (String, i64) = conn
            .query_row(
                "SELECT COALESCE(context_summary, ''), summarized_count FROM sessions WHERE id='probe'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("事故查询必须恢复可用");
        println!("插入成功，摘要='{}'，水位={}", summary, count);

        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        println!("修复后会话总数：{}（含探针行）", sessions);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partially_applied_migration_is_rolled_back() {
        let dir = temp_dir("rollback");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = open_connection(&dir).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_version (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )
        .unwrap();

        // 模拟"迁移执行到一半失败"：同一批 SQL 里有语法错误
        let tx = conn.unchecked_transaction().unwrap();
        let failed = tx.execute_batch(
            "ALTER TABLE sessions ADD COLUMN half_applied TEXT;
             THIS IS NOT VALID SQL;",
        );
        assert!(failed.is_err());
        drop(tx); // 未 commit → 整批回滚

        // 回滚后列不存在，因此重新跑迁移不会撞 duplicate column
        run_migrations(&conn).expect("migrations must run cleanly after a rollback");
        assert_eq!(schema_version(&conn), latest_version());
        // 失败的那一列确实没有残留
        assert!(conn
            .query_row("SELECT half_applied FROM sessions LIMIT 1", [], |r| r
                .get::<_, String>(0))
            .is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
