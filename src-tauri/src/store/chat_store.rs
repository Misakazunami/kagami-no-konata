use anyhow::Result;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

use crate::agent::context::{Message, Role, Session};
use crate::agent::notes::SessionNote;
use crate::agent::plan::SessionPlan;

/// 一个会话最多保留的计划项（与 `agent::plan::sanitize_items` 共用）
pub const MAX_PLAN_ITEMS: usize = 20;

/// 工作区改动快照的索引行（备份文件本体在 `{app_data_dir}/snapshots/`）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapshotRow {
    pub id: String,
    pub session_id: String,
    pub stream_id: String,
    pub root_id: String,
    pub rel_path: String,
    pub backup_name: String,
    pub bytes: i64,
    pub created_at: String,
}

/// 使用统计
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsageStats {
    pub total_requests: i64,
    pub total_tokens: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_time_ms: i64,
}

/// 消息回退 / 编辑造成的历史截断结果
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RewindOutcome {
    /// 实际删除的消息条数
    pub removed: usize,
    /// 被删除消息关联的工具轮次（供界面选择是否一并回滚这些轮次的文件改动）
    pub affected_streams: Vec<String>,
    /// 目标消息在会话中的插入序号（0-based，仅用于内部摘要水位判断）
    pub target_index: i64,
    /// 是否因为截断清空了持久化上下文摘要（摘要引用了已不存在的消息）
    pub summary_cleared: bool,
    /// 随截断一并清理的工作记忆条数（截断点之后写下的结论已不可信）
    #[serde(default)]
    pub notes_removed: usize,
    /// 计划里的"进行中"项是否被置为"受阻"（历史已被改写，原进度已无法继续）
    #[serde(default)]
    pub plan_blocked: bool,
}

/// 工具调用轨迹行（仅供 UI 回放，不参与模型上下文）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolInvocationRow {
    pub id: String,
    pub session_id: String,
    pub message_id: Option<String>,
    pub stream_id: String,
    pub step: i64,
    pub tool_name: String,
    pub tool_label: String,
    pub arguments_json: String,
    pub status: String,
    pub result_preview: Option<String>,
    pub error: Option<String>,
    pub truncated: bool,
    pub duration_ms: i64,
    pub approval: Option<String>,
    /// 工具自算的额外 token 估算（子代理等"隐藏开销"；旧库默认 0）
    #[serde(default)]
    pub extra_tokens: i64,
    pub created_at: String,
}

/// 对话持久化存储
pub struct ChatStore {
    conn: Connection,
}

/// 消息行映射（供多个查询方法复用）
fn map_message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let role_str: String = row.get(2)?;
    let role = match role_str.as_str() {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => Role::User,
    };
    Ok(Message {
        id: row.get(0)?,
        session_id: row.get(1)?,
        role,
        content: row.get(3)?,
        timestamp: row.get(4)?,
        token_count: row.get(5).unwrap_or(0),
        thinking_ms: row.get(6).unwrap_or(0),
        thinking: row.get(7).ok(),
        model: row.get(8).ok(),
    })
}

const MESSAGE_COLUMNS: &str =
    "id, session_id, role, content, created_at, token_count, thinking_ms, thinking, model";

/// 会话行的列清单（新增列请同时改这里与 [`map_session_row`]）
const SESSION_COLUMNS: &str = "id, title, persona_id, session_type, task_mode, workspace_id, \
     model_pref, auto_approve_all, created_at, updated_at";

/// 带表别名的列清单（`find_latest_empty_session` 的 JOIN 查询用）
const SESSION_COLUMNS_ALIASED: &str = "s.id, s.title, s.persona_id, s.session_type, s.task_mode, \
     s.workspace_id, s.model_pref, s.auto_approve_all, s.created_at, s.updated_at";

/// 会话行映射（供多个查询方法复用）
///
/// `model_pref` 是 JSON 文本：解析失败一律当作"未设置"并记日志，
/// 而不是让整个会话列表查询失败（辅助状态绝不能拖垮主功能）。
fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let raw: Option<String> = row.get(6)?;
    let model_pref = decode_model_pref(raw);

    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        persona_id: row.get(2)?,
        session_type: row.get(3)?,
        task_mode: row.get(4)?,
        workspace_id: row.get(5)?,
        model_pref,
        // 旧库由 ensure_schema 补列（DEFAULT 0），永远不会读不到
        auto_approve_all: row.get(7).unwrap_or(false),
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

/// 会话模型偏好 → JSON 文本（空偏好不写库，保持 `NULL` 语义）
fn encode_model_pref(pref: Option<&crate::llm::router::SessionModelPref>) -> Option<String> {
    let pref = pref.filter(|p| !p.is_empty())?;
    serde_json::to_string(pref).ok()
}

/// JSON 文本 → 会话模型偏好（脏数据降级为"未设置"）
fn decode_model_pref(raw: Option<String>) -> Option<crate::llm::router::SessionModelPref> {
    let text = raw?;
    if text.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<crate::llm::router::SessionModelPref>(&text) {
        Ok(pref) => Some(pref),
        Err(e) => {
            eprintln!("[models] 会话模型偏好解析失败，按「跟随全局提供商」处理: {}", e);
            None
        }
    }
}

impl ChatStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// 创建新会话
    pub fn create_session(
        &self,
        persona_id: &str,
        title: &str,
        session_type: Option<&str>,
        task_mode: Option<&str>,
        workspace_id: Option<&str>,
    ) -> Result<Session> {
        self.create_session_with_model(
            persona_id,
            title,
            session_type,
            task_mode,
            workspace_id,
            None,
        )
    }

    /// 创建新会话（带会话级模型偏好）
    pub fn create_session_with_model(
        &self,
        persona_id: &str,
        title: &str,
        session_type: Option<&str>,
        task_mode: Option<&str>,
        workspace_id: Option<&str>,
        model_pref: Option<&crate::llm::router::SessionModelPref>,
    ) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let s_type = session_type.unwrap_or("chat");
        let t_mode = task_mode.unwrap_or("plan");
        // 归一化：全空的偏好等同"未设置"，落库为 NULL，返回的 Session 也必须一致
        let model_pref = model_pref.filter(|p| !p.is_empty());
        let pref_json = encode_model_pref(model_pref);

        self.conn.execute(
            "INSERT INTO sessions (id, title, persona_id, session_type, task_mode, workspace_id, model_pref, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![id, title, persona_id, s_type, t_mode, workspace_id, pref_json, now, now],
        )?;

        Ok(Session {
            id,
            title: title.to_string(),
            persona_id: persona_id.to_string(),
            session_type: s_type.to_string(),
            task_mode: t_mode.to_string(),
            workspace_id: workspace_id.map(|w| w.to_string()),
            model_pref: model_pref.cloned(),
            // 新会话默认关闭 AUTO（INSERT 靠列默认值 0）
            auto_approve_all: false,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// 添加消息（带元数据，事务保证消息与会话更新原子性）
    ///
    /// `model` 只对 assistant 消息有意义（会话级模型选择的展示标签），
    /// 用户/系统消息传 `None`。
    // 参数个数由"消息元数据"的字段决定，拆结构体只会把调用点变啰嗦
    #[allow(clippy::too_many_arguments)]
    pub fn add_message(
        &self,
        session_id: &str,
        role: Role,
        content: &str,
        token_count: i64,
        thinking_ms: i64,
        thinking: Option<String>,
        model: Option<&str>,
    ) -> Result<Message> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let date_key = now.format("%Y-%m-%d").to_string();
        let timestamp = now.to_rfc3339();

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO messages (id, session_id, role, content, created_at, date_key, token_count, thinking_ms, thinking, model) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![id, session_id, role.to_string(), content, timestamp, date_key, token_count, thinking_ms, thinking, model],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![timestamp, session_id],
        )?;
        tx.commit()?;

        Ok(Message {
            id,
            role,
            content: content.to_string(),
            timestamp,
            session_id: session_id.to_string(),
            token_count,
            thinking_ms,
            thinking,
            model: model.map(|m| m.to_string()),
        })
    }

    /// 获取会话的所有消息（含元数据）
    pub fn get_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            &format!(
                "SELECT {} FROM messages WHERE session_id = ?1 ORDER BY created_at",
                MESSAGE_COLUMNS
            ),
        )?;

        let messages = stmt
            .query_map(rusqlite::params![session_id], map_message_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(messages)
    }

    /// 获取会话最近 N 条消息（按时间正序返回，用于构建上下文窗口）
    pub fn get_recent_messages(&self, session_id: &str, limit: usize) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM messages WHERE session_id = ?1 ORDER BY created_at DESC LIMIT ?2",
            MESSAGE_COLUMNS
        ))?;

        let mut messages: Vec<Message> = stmt
            .query_map(rusqlite::params![session_id, limit as i64], map_message_row)?
            .collect::<Result<Vec<_>, _>>()?;
        messages.reverse();
        Ok(messages)
    }

    /// 按序号区间取消息切片（offset 起 limit 条，用于增量摘要）
    pub fn get_messages_slice(
        &self,
        session_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM messages WHERE session_id = ?1 ORDER BY created_at LIMIT ?2 OFFSET ?3",
            MESSAGE_COLUMNS
        ))?;
        let messages = stmt
            .query_map(
                rusqlite::params![session_id, limit, offset],
                map_message_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    /// 统计会话消息总数
    pub fn count_messages(&self, session_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?)
    }

    /// 获取会话的用户消息中指定时间之后的记录（正序，用于增量记忆提取）
    pub fn get_user_messages_after(
        &self,
        session_id: &str,
        after: &str,
    ) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM messages WHERE session_id = ?1 AND role = 'user' AND created_at > ?2 ORDER BY created_at",
            MESSAGE_COLUMNS
        ))?;
        let messages = stmt
            .query_map(rusqlite::params![session_id, after], map_message_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(messages)
    }

    /// 获取会话最近 N 条用户消息（正序返回，首次提取时限制范围）
    pub fn get_recent_user_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM messages WHERE session_id = ?1 AND role = 'user' ORDER BY created_at DESC LIMIT ?2",
            MESSAGE_COLUMNS
        ))?;
        let mut messages: Vec<Message> = stmt
            .query_map(rusqlite::params![session_id, limit as i64], map_message_row)?
            .collect::<Result<Vec<_>, _>>()?;
        messages.reverse();
        Ok(messages)
    }

    // ─── 消息回退 / 编辑（历史截断） ─────────────────────

    /// 按 id 取单条消息（回退 / 重试定位用，避免整表回读）
    pub fn get_message(&self, session_id: &str, message_id: &str) -> Result<Message> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM messages WHERE id = ?1 AND session_id = ?2",
            MESSAGE_COLUMNS
        ))?;
        let mut rows = stmt.query_map(rusqlite::params![message_id, session_id], map_message_row)?;
        match rows.next() {
            Some(row) => Ok(row?),
            None => anyhow::bail!("消息不存在或不属于该会话：{}", message_id),
        }
    }

    /// 某条消息之前最近的一条用户消息（重试回复时复用原提问，避免产生重复行）
    pub fn last_user_message_before(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Option<Message>> {
        let rowid: i64 = self
            .conn
            .query_row(
                "SELECT rowid FROM messages WHERE id = ?1 AND session_id = ?2",
                rusqlite::params![message_id, session_id],
                |row| row.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    anyhow::anyhow!("消息不存在或不属于该会话：{}", message_id)
                }
                other => other.into(),
            })?;

        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM messages
              WHERE session_id = ?1 AND role = 'user' AND rowid < ?2
              ORDER BY rowid DESC LIMIT 1",
            MESSAGE_COLUMNS
        ))?;
        let mut rows = stmt.query_map(rusqlite::params![session_id, rowid], map_message_row)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// 定位一条消息（按插入序），返回 `(rowid, role, content, created_at)`
    ///
    /// 用 `rowid` 而不是 `created_at` 做截断边界：时间戳是 RFC3339（亚秒级），
    /// 并发窗口下理论上可能并列；`rowid` 是 SQLite 的插入序，边界永远无歧义。
    fn locate_message(
        conn: &Connection,
        session_id: &str,
        message_id: &str,
    ) -> Result<(i64, String, String, String)> {
        conn.query_row(
            "SELECT rowid, role, content, created_at FROM messages WHERE id = ?1 AND session_id = ?2",
            rusqlite::params![message_id, session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                anyhow::anyhow!("消息不存在或不属于该会话：{}", message_id)
            }
            other => other.into(),
        })
    }

    /// 目标消息之前还有多少条消息（0-based 插入序号）
    fn message_index_before(conn: &Connection, session_id: &str, rowid: i64) -> Result<i64> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND rowid < ?2",
            rusqlite::params![session_id, rowid],
            |row| row.get(0),
        )?)
    }

    /// 事务内截断：删除目标消息之后（`inclusive=false`）或含目标（`inclusive=true`）的全部消息
    ///
    /// 同时级联清理这些消息关联的工具轨迹，并校正上下文摘要水位：
    /// - 被删/被编辑的消息落在已摘要区间（`target_index < summarized_count`）时，
    ///   摘要正文里包含已不存在的内容，必须整体清空重算；
    /// - 否则水位无需变化（被删的都是摘要覆盖范围之外的消息）。
    ///
    /// 还要清理两类"跟随历史"的跨轮状态：截断点之后写下的工作记忆（引用了
    /// 已删除的内容，注入下一轮只会误导模型），以及计划里"进行中"的条目
    /// （历史被改写后已不可能继续，置为"受阻"而不是留在"进行中"）。
    #[allow(clippy::type_complexity)]
    fn truncate_from_tx(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
        target_rowid: i64,
        target_created_at: &str,
        target_index: i64,
        inclusive: bool,
    ) -> Result<(usize, Vec<String>, bool, usize, bool)> {
        let op = if inclusive { ">=" } else { ">" };
        let ids_subquery = format!(
            "SELECT id FROM messages WHERE session_id = ?1 AND rowid {} ?2",
            op
        );
        let affected_sql = format!(
            "SELECT DISTINCT stream_id FROM tool_invocations
              WHERE session_id = ?1
                AND (message_id IN ({})
                     OR (message_id IS NULL AND created_at >= ?3))",
            ids_subquery
        );
        let affected_streams: Vec<String> = {
            let mut stmt = tx.prepare(&affected_sql)?;
            let rows = stmt
                .query_map(
                    rusqlite::params![session_id, target_rowid, target_created_at],
                    |row| row.get(0),
                )?
                .collect::<Result<Vec<String>, _>>()?;
            rows
        };

        tx.execute(
            &format!(
                "DELETE FROM tool_invocations
                  WHERE session_id = ?1
                    AND (message_id IN ({})
                         OR (message_id IS NULL AND created_at >= ?3))",
                ids_subquery
            ),
            rusqlite::params![session_id, target_rowid, target_created_at],
        )?;

        let removed = tx.execute(
            &format!(
                "DELETE FROM messages WHERE session_id = ?1 AND rowid {} ?2",
                op
            ),
            rusqlite::params![session_id, target_rowid],
        )?;

        // 截断点之后写下的工作记忆：它们引用的工具输出/结论已经不在历史里。
        //
        // 注意边界不能用 target_created_at：assistant 消息是在整轮生成**结束后**
        // 才落库的，本轮工具里写下的笔记时间戳比它更早。正确的界是"第一条被删
        // 消息之前的那条消息"——本轮笔记一定晚于它。
        let boundary_op = if inclusive { "<" } else { "<=" };
        let boundary: Option<String> = tx
            .query_row(
                &format!(
                    "SELECT created_at FROM messages
                     WHERE session_id = ?1 AND rowid {} ?2
                     ORDER BY rowid DESC LIMIT 1",
                    boundary_op
                ),
                rusqlite::params![session_id, target_rowid],
                |row| row.get(0),
            )
            .optional()?;
        let notes_removed = match boundary {
            Some(boundary) => tx.execute(
                "DELETE FROM tool_notes WHERE session_id = ?1 AND created_at > ?2",
                rusqlite::params![session_id, boundary],
            )?,
            // 截断点之前没有任何消息：整个会话的笔记都属于被删区间
            None => tx.execute("DELETE FROM tool_notes WHERE session_id = ?1", [session_id])?,
        };

        // 计划：把"进行中"置为"受阻"（保留已完成/待办项，用户能看到真实状态）
        let mut plan_blocked = false;
        if removed > 0 {
            let raw: Option<String> = tx
                .query_row(
                    "SELECT plan_json FROM session_plans WHERE session_id = ?1",
                    [session_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(raw) = raw {
                if let Some(mut plan) = SessionPlan::from_json(&raw) {
                    let mut changed = false;
                    for item in &mut plan.items {
                        if item.status == crate::agent::plan::PlanStatus::Doing {
                            item.status = crate::agent::plan::PlanStatus::Blocked;
                            changed = true;
                        }
                    }
                    if changed {
                        plan.updated_at = Utc::now().to_rfc3339();
                        tx.execute(
                            "UPDATE session_plans SET plan_json = ?1, updated_at = ?2 WHERE session_id = ?3",
                            rusqlite::params![plan.to_json(), plan.updated_at, session_id],
                        )?;
                        plan_blocked = true;
                    }
                }
            }
        }

        let new_total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        let summarized_count: i64 = tx.query_row(
            "SELECT COALESCE(summarized_count, 0) FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )?;

        let mut summary_cleared = false;
        if target_index < summarized_count {
            tx.execute(
                "UPDATE sessions SET context_summary = NULL, summarized_count = 0 WHERE id = ?1",
                [session_id],
            )?;
            summary_cleared = true;
        } else if summarized_count > new_total {
            // 防御性兜底（正常不会走到，除非历史数据本身的水位就偏高）
            tx.execute(
                "UPDATE sessions SET summarized_count = ?1 WHERE id = ?2",
                rusqlite::params![new_total, session_id],
            )?;
        }

        Ok((
            removed,
            affected_streams,
            summary_cleared,
            notes_removed,
            plan_blocked,
        ))
    }

    /// 回退预览（只读）：删除范围与受影响的快照轮次，供确认弹窗展示
    pub fn preview_rewind(
        &self,
        session_id: &str,
        message_id: &str,
        inclusive: bool,
    ) -> Result<RewindOutcome> {
        let (target_rowid, _role, _content, target_created_at) =
            Self::locate_message(&self.conn, session_id, message_id)?;
        let target_index = Self::message_index_before(&self.conn, session_id, target_rowid)?;
        let op = if inclusive { ">=" } else { ">" };

        let removed: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1 AND rowid {} ?2",
                op
            ),
            rusqlite::params![session_id, target_rowid],
            |row| row.get(0),
        )?;

        let affected_streams: Vec<String> = {
            let mut stmt = self.conn.prepare(&format!(
                "SELECT DISTINCT stream_id FROM tool_invocations
                  WHERE session_id = ?1
                    AND (message_id IN (SELECT id FROM messages WHERE session_id = ?1 AND rowid {} ?2)
                         OR (message_id IS NULL AND created_at >= ?3))",
                op
            ))?;
            let rows = stmt
                .query_map(
                    rusqlite::params![session_id, target_rowid, target_created_at],
                    |row| row.get(0),
                )?
                .collect::<Result<Vec<String>, _>>()?;
            rows
        };

        Ok(RewindOutcome {
            removed: removed as usize,
            affected_streams,
            target_index,
            summary_cleared: false,
            notes_removed: 0,
            plan_blocked: false,
        })
    }

    /// 回退（含删除该条）：删除目标消息及其之后的全部消息
    ///
    /// `inclusive=false` 时保留目标消息本身（"重试用户提问"复用原行，避免产生重复提问）。
    pub fn rewind_messages(
        &self,
        session_id: &str,
        message_id: &str,
        inclusive: bool,
    ) -> Result<RewindOutcome> {
        let tx = self.conn.unchecked_transaction()?;
        let (target_rowid, _role, _content, target_created_at) =
            Self::locate_message(&tx, session_id, message_id)?;
        let target_index = Self::message_index_before(&tx, session_id, target_rowid)?;
        let (removed, affected_streams, summary_cleared, notes_removed, plan_blocked) =
            Self::truncate_from_tx(
                &tx,
                session_id,
                target_rowid,
                &target_created_at,
                target_index,
                inclusive,
            )?;
        if removed > 0 {
            tx.execute(
                "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
                rusqlite::params![Utc::now().to_rfc3339(), session_id],
            )?;
        }
        tx.commit()?;
        Ok(RewindOutcome {
            removed,
            affected_streams,
            target_index,
            summary_cleared,
            notes_removed,
            plan_blocked,
        })
    }

    /// 编辑用户消息正文（重算 token）并截断其后全部消息
    ///
    /// 只允许编辑 `user` 消息：assistant 消息是模型输出的历史记录，
    /// 改写它会让后续对话的因果变得不可信（界面提供"重试"而非编辑）。
    pub fn edit_user_message(
        &self,
        session_id: &str,
        message_id: &str,
        content: &str,
        token_count: i64,
    ) -> Result<RewindOutcome> {
        let tx = self.conn.unchecked_transaction()?;
        let (target_rowid, role, _old_content, target_created_at) =
            Self::locate_message(&tx, session_id, message_id)?;
        if role != "user" {
            anyhow::bail!("只能编辑用户消息");
        }
        tx.execute(
            "UPDATE messages SET content = ?1, token_count = ?2 WHERE id = ?3 AND session_id = ?4",
            rusqlite::params![content, token_count, message_id, session_id],
        )?;
        let target_index = Self::message_index_before(&tx, session_id, target_rowid)?;
        let (removed, affected_streams, summary_cleared, notes_removed, plan_blocked) =
            Self::truncate_from_tx(
                &tx,
                session_id,
                target_rowid,
                &target_created_at,
                target_index,
                false,
            )?;
        tx.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![Utc::now().to_rfc3339(), session_id],
        )?;
        tx.commit()?;
        Ok(RewindOutcome {
            removed,
            affected_streams,
            target_index,
            summary_cleared,
            notes_removed,
            plan_blocked,
        })
    }

    // ─── 会话摘要与提取水位 ─────────────────────────────

    /// 获取持久化上下文摘要
    pub fn get_context_summary(&self, session_id: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT context_summary FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        ) {
            Ok(v) => Ok(v.filter(|s| !s.trim().is_empty())),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 获取摘要与已摘要消息数（无记录时返回空串和 0）
    pub fn get_summary_with_count(&self, session_id: &str) -> Result<(String, i64)> {
        match self.conn.query_row(
            "SELECT COALESCE(context_summary, ''), summarized_count FROM sessions WHERE id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ) {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((String::new(), 0)),
            Err(e) => Err(e.into()),
        }
    }

    /// 写入摘要与已摘要消息数
    pub fn set_session_summary(
        &self,
        session_id: &str,
        summary: &str,
        summarized_count: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET context_summary = ?1, summarized_count = ?2 WHERE id = ?3",
            rusqlite::params![summary, summarized_count, session_id],
        )?;
        Ok(())
    }

    /// 获取记忆提取水位线
    pub fn get_last_extracted_at(&self, session_id: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT last_extracted_at FROM sessions WHERE id = ?1",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        ) {
            Ok(v) => Ok(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 推进记忆提取水位线
    pub fn set_last_extracted_at(&self, session_id: &str, ts: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_extracted_at = ?1 WHERE id = ?2",
            rusqlite::params![ts, session_id],
        )?;
        Ok(())
    }

    /// 列出所有会话
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM sessions ORDER BY updated_at DESC",
            SESSION_COLUMNS
        ))?;

        let sessions = stmt
            .query_map([], map_session_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(sessions)
    }

    /// 获取单个会话信息
    pub fn get_session(&self, session_id: &str) -> Result<Session> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {} FROM sessions WHERE id = ?1",
                    SESSION_COLUMNS
                ),
                rusqlite::params![session_id],
                map_session_row,
            )
            .map_err(|e| e.into())
    }

    /// 设置会话级模型偏好（`None` = 清除，回到"跟随全局提供商"）
    pub fn set_session_model_pref(
        &self,
        session_id: &str,
        pref: Option<&crate::llm::router::SessionModelPref>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE sessions SET model_pref = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![encode_model_pref(pref), now, session_id],
        )?;
        Ok(())
    }

    /// 设置会话的任务模式 (plan / work)
    pub fn set_task_mode(&self, session_id: &str, task_mode: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE sessions SET task_mode = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![task_mode, now, session_id],
        )?;
        Ok(())
    }

    /// 设置会话级 AUTO（自动允许所有需要审批的工具调用）
    ///
    /// 是否允许开启由命令层校验（仅任务会话）；这里只负责持久化。
    pub fn set_session_auto_approve(&self, session_id: &str, enabled: bool) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE sessions SET auto_approve_all = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![enabled, now, session_id],
        )?;
        Ok(())
    }

    /// 按日期查找会话（限指定类型：任务会话不能被"今日聊天"复用）
    ///
    /// `created_at` 存的是 UTC RFC3339，而调用方的"今天"来自 `Local::now()`：
    /// SQL 必须用 `localtime` 修饰符换算，否则 UTC+8 环境下每天 00:00–08:00
    /// 创建的会话都匹配不到"今日"，导致重复建会话与错误的人格解析。
    pub fn find_session_by_date(&self, date: &str, session_type: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM sessions
             WHERE date(created_at, 'localtime') = ?1 AND session_type = ?2
             ORDER BY updated_at DESC LIMIT 1",
            SESSION_COLUMNS
        ))?;

        let mut rows = stmt.query_map(rusqlite::params![date, session_type], map_session_row)?;

        match rows.next() {
            Some(session) => Ok(Some(session?)),
            None => Ok(None),
        }
    }

    /// 查找最近的空会话（无消息，不限日期；限指定类型）
    ///
    /// 不过滤 `session_type` 时，一个空的任务会话会被"今日聊天"入口
    /// （或悬浮窗）复用，用户会发现桌宠在任务工程里说话。
    pub fn find_latest_empty_session(&self, session_type: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {}
             FROM sessions s
             LEFT JOIN messages m ON s.id = m.session_id
             WHERE s.session_type = ?1
             GROUP BY s.id
             HAVING COUNT(m.id) = 0
             ORDER BY s.created_at DESC
             LIMIT 1",
            SESSION_COLUMNS_ALIASED
        ))?;

        let mut rows = stmt.query_map([session_type], map_session_row)?;

        match rows.next() {
            Some(session) => Ok(Some(session?)),
            None => Ok(None),
        }
    }

    /// 更新会话标题
    pub fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![title, Utc::now().to_rfc3339(), session_id],
        )?;
        Ok(())
    }

    /// 设置/清除会话绑定的工作区（`None` = 使用默认沙箱）
    pub fn set_session_workspace(&self, session_id: &str, workspace_id: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET workspace_id = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![workspace_id, Utc::now().to_rfc3339(), session_id],
        )?;
        Ok(())
    }

    /// 删除会话（事务保证消息与会话原子删除）
    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        tx.execute(
            "DELETE FROM sessions WHERE id = ?1",
            rusqlite::params![session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 记录使用统计
    pub fn record_usage(
        &self,
        prompt_tokens: i64,
        completion_tokens: i64,
        time_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE usage_stats SET
                total_requests = total_requests + 1,
                total_tokens = total_tokens + ?1,
                prompt_tokens = prompt_tokens + ?2,
                completion_tokens = completion_tokens + ?3,
                total_time_ms = total_time_ms + ?4,
                updated_at = datetime('now')
            WHERE id = 1",
            rusqlite::params![
                prompt_tokens + completion_tokens,
                prompt_tokens,
                completion_tokens,
                time_ms,
            ],
        )?;
        Ok(())
    }

    /// 获取使用统计
    pub fn get_usage_stats(&self) -> Result<UsageStats> {
        self.conn.query_row(
            "SELECT total_requests, total_tokens, prompt_tokens, completion_tokens, total_time_ms FROM usage_stats WHERE id = 1",
            [],
            |row| {
                Ok(UsageStats {
                    total_requests: row.get(0)?,
                    total_tokens: row.get(1)?,
                    prompt_tokens: row.get(2)?,
                    completion_tokens: row.get(3)?,
                    total_time_ms: row.get(4)?,
                })
            },
        ).map_err(|e| e.into())
    }

    /// 重置使用统计
    pub fn reset_usage_stats(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE usage_stats SET total_requests = 0, total_tokens = 0, prompt_tokens = 0, completion_tokens = 0, total_time_ms = 0, updated_at = datetime('now') WHERE id = 1",
            [],
        )?;
        Ok(())
    }

    // ─── 工具调用轨迹 ──────────────────────────────────

    /// 记录一轮生成中的工具调用
    ///
    /// 只保存预览文本：完整结果可能包含整份文件内容，落库既无必要也
    /// 有泄漏风险（例如命令输出里的路径/密钥）。
    pub fn record_tool_invocations(&self, rows: &[ToolInvocationRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        for row in rows {
            tx.execute(
                "INSERT OR REPLACE INTO tool_invocations
                    (id, session_id, message_id, stream_id, step, tool_name, tool_label,
                     arguments_json, status, result_preview, error, truncated, duration_ms,
                     approval, extra_tokens, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                rusqlite::params![
                    row.id,
                    row.session_id,
                    row.message_id,
                    row.stream_id,
                    row.step,
                    row.tool_name,
                    row.tool_label,
                    row.arguments_json,
                    row.status,
                    row.result_preview,
                    row.error,
                    row.truncated as i64,
                    row.duration_ms,
                    row.approval,
                    row.extra_tokens,
                    row.created_at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 把某个 stream 的轨迹挂到落位后的 assistant 消息上
    pub fn attach_tool_invocations_to_message(
        &self,
        stream_id: &str,
        message_id: &str,
    ) -> Result<usize> {
        let updated = self.conn.execute(
            "UPDATE tool_invocations SET message_id = ?2 WHERE stream_id = ?1",
            rusqlite::params![stream_id, message_id],
        )?;
        Ok(updated)
    }

    /// 读取某个会话的全部工具轨迹（正序）
    pub fn get_tool_invocations(&self, session_id: &str) -> Result<Vec<ToolInvocationRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, message_id, stream_id, step, tool_name, tool_label,
                    arguments_json, status, result_preview, error, truncated, duration_ms,
                    approval, extra_tokens, created_at
             FROM tool_invocations WHERE session_id = ?1 ORDER BY created_at ASC, step ASC",
        )?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok(ToolInvocationRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    message_id: row.get(2).ok(),
                    stream_id: row.get(3)?,
                    step: row.get(4)?,
                    tool_name: row.get(5)?,
                    tool_label: row.get(6)?,
                    arguments_json: row.get(7)?,
                    status: row.get(8)?,
                    result_preview: row.get(9).ok(),
                    error: row.get(10).ok(),
                    truncated: row.get::<_, i64>(11).unwrap_or(0) != 0,
                    duration_ms: row.get(12).unwrap_or(0),
                    approval: row.get(13).ok(),
                    extra_tokens: row.get(14).unwrap_or(0),
                    created_at: row.get(15)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ─── 会话级任务计划（`update_plan` 工具写，system prompt 与界面读） ───

    /// 覆盖写入计划；空计划等于删除该行（避免留下"看似有计划"的空壳）
    pub fn save_plan(&self, session_id: &str, plan: &SessionPlan) -> Result<()> {
        if plan.is_empty() {
            return self.clear_plan(session_id);
        }
        self.conn.execute(
            "INSERT INTO session_plans (session_id, plan_json, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                plan_json = excluded.plan_json,
                updated_at = excluded.updated_at",
            rusqlite::params![session_id, plan.to_json(), plan.updated_at],
        )?;
        Ok(())
    }

    /// 读取计划；缺失或数据损坏都返回 `None`（调用方按"没有计划"处理）
    pub fn get_plan(&self, session_id: &str) -> Result<Option<SessionPlan>> {
        let mut stmt = self
            .conn
            .prepare("SELECT plan_json FROM session_plans WHERE session_id = ?1")?;
        let mut rows = stmt.query([session_id])?;
        match rows.next()? {
            Some(row) => {
                let raw: String = row.get(0)?;
                Ok(SessionPlan::from_json(&raw).filter(|plan| !plan.is_empty()))
            }
            None => Ok(None),
        }
    }

    pub fn clear_plan(&self, session_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM session_plans WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(())
    }

    // ─── 工作记忆（`save_note` 写、system prompt 读） ───

    /// 写入一条工作记忆，并按上限淘汰最旧的条目
    ///
    /// 淘汰规则：先按**条数**（最多 [`crate::agent::notes::MAX_NOTES`] 条），
    /// 再按**总字节**（最多 `MAX_TOTAL_BYTES`）。淘汰与写入在**同一事务**内：
    /// 否则崩溃/失败会留下超限数据，直到下一次写入才被修掉。
    pub fn save_note(&self, session_id: &str, note: &SessionNote) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO tool_notes (id, session_id, title, content, bytes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                note.id,
                session_id,
                note.title,
                note.content,
                note.bytes as i64,
                note.created_at,
            ],
        )?;
        Self::trim_notes_conn(&tx, session_id)?;
        tx.commit()?;
        Ok(())
    }

    /// 按条数与总量上限淘汰超出部分（最旧的先走）
    pub fn trim_notes(&self, session_id: &str) -> Result<()> {
        Self::trim_notes_conn(&self.conn, session_id)
    }

    /// `trim_notes` 的连接版实现（供事务内调用）
    fn trim_notes_conn(conn: &Connection, session_id: &str) -> Result<()> {
        let notes = Self::list_notes_conn(conn, session_id)?;
        let mut keep_bytes = 0usize;
        // list_notes 按时间正序：从新往旧保留
        let mut keep: Vec<&SessionNote> = Vec::new();
        for note in notes.iter().rev() {
            let within_count = keep.len() < crate::agent::notes::MAX_NOTES;
            let within_bytes =
                keep_bytes + note.bytes <= crate::agent::notes::MAX_TOTAL_BYTES;
            if within_count && within_bytes {
                keep_bytes += note.bytes;
                keep.push(note);
            }
        }
        let keep_ids: Vec<String> = keep.iter().map(|note| note.id.clone()).collect();
        for note in &notes {
            if !keep_ids.contains(&note.id) {
                conn.execute("DELETE FROM tool_notes WHERE id = ?1", [&note.id])?;
            }
        }
        Ok(())
    }

    /// 读取某个会话的工作记忆（按时间正序，最旧的在前）
    pub fn list_notes(&self, session_id: &str) -> Result<Vec<SessionNote>> {
        Self::list_notes_conn(&self.conn, session_id)
    }

    fn list_notes_conn(conn: &Connection, session_id: &str) -> Result<Vec<SessionNote>> {
        let mut stmt = conn.prepare(
            "SELECT id, title, content, bytes, created_at
             FROM tool_notes WHERE session_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok(SessionNote {
                    id: row.get(0)?,
                    title: row.get(1).ok(),
                    content: row.get(2)?,
                    bytes: row.get::<_, i64>(3).unwrap_or(0).max(0) as usize,
                    created_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 删除一条（或全部）工作记忆；`id` 为 `None` 表示清空该会话
    pub fn delete_note(&self, session_id: &str, id: Option<&str>) -> Result<usize> {
        let removed = match id {
            Some(id) => self.conn.execute(
                "DELETE FROM tool_notes WHERE session_id = ?1 AND id = ?2",
                rusqlite::params![session_id, id],
            )?,
            None => self.conn.execute(
                "DELETE FROM tool_notes WHERE session_id = ?1",
                [session_id],
            )?,
        };
        Ok(removed)
    }

    // ─── 工作区改动快照（回滚用） ───

    /// 记录一条备份索引
    pub fn record_snapshot(&self, row: &SnapshotRow) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO workspace_snapshots
                (id, session_id, stream_id, root_id, rel_path, backup_name, bytes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                row.id,
                row.session_id,
                row.stream_id,
                row.root_id,
                row.rel_path,
                row.backup_name,
                row.bytes,
                row.created_at,
            ],
        )?;
        Ok(())
    }

    /// 某个 stream 已记录的备份（按记录顺序）
    pub fn list_snapshots(&self, stream_id: &str) -> Result<Vec<SnapshotRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, stream_id, root_id, rel_path, backup_name, bytes, created_at
             FROM workspace_snapshots WHERE stream_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map([stream_id], |row| {
                Ok(SnapshotRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    stream_id: row.get(2)?,
                    root_id: row.get(3)?,
                    rel_path: row.get(4)?,
                    backup_name: row.get(5)?,
                    bytes: row.get(6).unwrap_or(0),
                    created_at: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 删除某个 stream 的备份索引（备份文件由调用方清理）
    pub fn delete_snapshots(&self, stream_id: &str) -> Result<usize> {
        let removed = self.conn.execute(
            "DELETE FROM workspace_snapshots WHERE stream_id = ?1",
            [stream_id],
        )?;
        Ok(removed)
    }

    /// 某个会话的全部备份索引（按时间倒序；界面用它列出"本次任务改动过哪些文件"）
    pub fn list_session_snapshots(&self, session_id: &str) -> Result<Vec<SnapshotRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, stream_id, root_id, rel_path, backup_name, bytes, created_at
             FROM workspace_snapshots WHERE session_id = ?1
             ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok(SnapshotRow {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    stream_id: row.get(2)?,
                    root_id: row.get(3)?,
                    rel_path: row.get(4)?,
                    backup_name: row.get(5)?,
                    bytes: row.get(6).unwrap_or(0),
                    created_at: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 某个会话涉及的所有 stream_id（去重）
    ///
    /// 删除会话前调用：表行会随会话级联删除，但备份文件在磁盘上，
    /// 必须先把 stream 清单取出来交给调用方清理，否则文件永远回收不了。
    pub fn list_snapshot_streams(&self, session_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT stream_id FROM workspace_snapshots WHERE session_id = ?1",
        )?;
        let rows = stmt
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 清理过期的备份索引（保留最近 `keep_days` 天）
    ///
    /// 备份文件是磁盘上的真金白银，必须有回收机制，否则用户每改一次文件就多留一份。
    pub fn prune_snapshots(&self, keep_days: i64) -> Result<Vec<String>> {
        let cutoff = (Utc::now() - chrono::Duration::days(keep_days)).to_rfc3339();
        let streams: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT stream_id FROM workspace_snapshots WHERE created_at < ?1",
            )?;
            let rows = stmt
                .query_map([&cutoff], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for stream in &streams {
            self.delete_snapshots(stream)?;
        }
        Ok(streams)
    }

    // ─── 会话级持久授权（审批弹窗的「本会话允许」） ───

    /// 记录一条会话级授权（同工具重复授权幂等）
    pub fn grant_session_tool(&self, session_id: &str, tool: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO session_grants (session_id, tool, created_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![session_id, tool, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// 某个会话已授权的工具名（按字母序，界面直接展示）
    pub fn list_session_grants(&self, session_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT tool FROM session_grants WHERE session_id = ?1 ORDER BY tool ASC")?;
        let rows = stmt
            .query_map([session_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 撤销单个工具的会话授权，返回是否真的删掉了一条
    pub fn revoke_session_tool(&self, session_id: &str, tool: &str) -> Result<bool> {
        let removed = self.conn.execute(
            "DELETE FROM session_grants WHERE session_id = ?1 AND tool = ?2",
            rusqlite::params![session_id, tool],
        )?;
        Ok(removed > 0)
    }

    /// 会话级 token 用量：`(消息正文, 工具的隐藏开销)`
    ///
    /// 隐藏开销来自子代理等工具的 `extra_tokens`；两者分开返回，
    /// 界面可以如实展示"回答本身花了多少、后台调查又花了多少"。
    pub fn session_token_usage(&self, session_id: &str) -> Result<(i64, i64)> {
        let messages: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(token_count), 0) FROM messages WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        let extra: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(extra_tokens), 0) FROM tool_invocations WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        Ok((messages.max(0), extra.max(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::ModelMode;
    use crate::llm::router::SessionModelPref;

    fn temp_store(tag: &str) -> (ChatStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "konata-store-{}-{}",
            tag,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let conn = crate::store::db::init_db(&dir).expect("init db");
        (ChatStore::new(conn), dir)
    }

    fn manual_pref() -> SessionModelPref {
        SessionModelPref {
            mode: ModelMode::Manual,
            provider_id: Some("p1".to_string()),
            model: Some("m1".to_string()),
            thinking: Some(true),
        }
    }

    /// 会话级模型偏好必须能穿过数据库往返，且**所有**读取路径都带上它
    #[test]
    fn model_pref_round_trips_through_every_read_path() {
        let (store, dir) = temp_store("model-pref");
        let pref = manual_pref();
        let session = store
            .create_session_with_model(
                "persona",
                "标题",
                Some("task"),
                Some("plan"),
                None,
                Some(&pref),
            )
            .expect("create");
        assert_eq!(session.model_pref.as_ref(), Some(&pref));

        assert_eq!(
            store.get_session(&session.id).unwrap().model_pref.as_ref(),
            Some(&pref),
            "单条查询"
        );
        assert_eq!(
            store.list_sessions().unwrap()[0].model_pref.as_ref(),
            Some(&pref),
            "列表查询"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// assistant 消息的模型标签必须落库并随所有读取路径返回
    /// （否则 `session-updated` 的全量回读会把它抹掉，用户永远看不到"这条是谁答的"）
    #[test]
    fn assistant_model_label_round_trips() {
        let (store, dir) = temp_store("model-label");
        let session = store
            .create_session("konata-default", "t", None, None, None)
            .unwrap();
        store
            .add_message(&session.id, Role::User, "你好", 1, 0, None, None)
            .unwrap();
        store
            .add_message(
                &session.id,
                Role::Assistant,
                "你好呀",
                2,
                0,
                None,
                Some("qwen3-32b（主模型）"),
            )
            .unwrap();

        let messages = store.get_messages(&session.id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].model, None, "用户消息没有模型标签");
        assert_eq!(messages[1].model.as_deref(), Some("qwen3-32b（主模型）"));

        // 上下文窗口读取路径同样带着它（结构一致，不会因缺列而失败）
        let recent = store.get_recent_messages(&session.id, 10).unwrap();
        assert_eq!(recent[1].model.as_deref(), Some("qwen3-32b（主模型）"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// `find_latest_empty_session` 用的是带表别名的列清单：列顺序必须与映射一致
    #[test]
    fn aliased_session_queries_expose_model_pref() {
        let (store, dir) = temp_store("aliased");
        let pref = manual_pref();
        let session = store
            .create_session_with_model("p", "t", Some("task"), Some("plan"), None, Some(&pref))
            .expect("create");

        let found = store
            .find_latest_empty_session("task")
            .expect("query ok")
            .expect("应当找到空会话");
        assert_eq!(found.id, session.id);
        assert_eq!(found.model_pref.as_ref(), Some(&pref));
        assert_eq!(found.session_type, "task", "其他列也不能错位");
        // 类型过滤：普通会话入口不能复用任务会话
        assert!(store.find_latest_empty_session("chat").unwrap().is_none());

        // `created_at` 存 UTC，但"今天"是用户本地概念：查询必须按 localtime
        // 换算，否则 UTC+8 的凌晨时段找不到刚创建的会话（真实缺陷）。
        let local_today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(store.find_session_by_date(&local_today, "task").unwrap().is_some());
        assert!(store.find_session_by_date(&local_today, "chat").unwrap().is_none());

        // 本地日期与 UTC 日期不同的时区里，用 UTC 日期查询必须查不到
        // （证明查询确实按本地时间解释，而不是碰巧同日）
        let utc_today = Utc::now().format("%Y-%m-%d").to_string();
        if utc_today != local_today {
            assert!(
                store.find_session_by_date(&utc_today, "task").unwrap().is_none(),
                "查询必须按本地日期解释 created_at"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 清除偏好 → `NULL`；空偏好不写库（避免留下无意义的 `{}`）
    #[test]
    fn empty_or_cleared_pref_is_stored_as_null() {
        let (store, dir) = temp_store("clear-pref");
        let session = store
            .create_session_with_model(
                "p",
                "t",
                Some("chat"),
                Some("plan"),
                None,
                Some(&SessionModelPref::default()),
            )
            .expect("create");
        assert!(session.model_pref.is_none(), "全空偏好等同未设置");

        store
            .set_session_model_pref(&session.id, Some(&manual_pref()))
            .expect("set");
        assert!(store.get_session(&session.id).unwrap().model_pref.is_some());

        store.set_session_model_pref(&session.id, None).expect("clear");
        assert!(
            store.get_session(&session.id).unwrap().model_pref.is_none(),
            "清除后必须回到「跟随全局」"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// AUTO 开关必须能穿过数据库往返，且默认关闭
    #[test]
    fn auto_approve_all_round_trips_and_defaults_off() {
        let (store, dir) = temp_store("auto-approve");
        let session = store
            .create_session("p", "t", Some("task"), Some("work"), None)
            .expect("create");
        assert!(!session.auto_approve_all, "新会话默认关闭 AUTO");
        assert!(!store.get_session(&session.id).unwrap().auto_approve_all);

        store
            .set_session_auto_approve(&session.id, true)
            .expect("set");
        assert!(store.get_session(&session.id).unwrap().auto_approve_all);
        assert!(store.list_sessions().unwrap()[0].auto_approve_all, "列表查询");

        store
            .set_session_auto_approve(&session.id, false)
            .expect("clear");
        assert!(!store.get_session(&session.id).unwrap().auto_approve_all);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 脏 JSON 只能降级成"未设置"，绝不能让会话读取失败
    #[test]
    fn corrupt_model_pref_degrades_instead_of_failing() {
        let (store, dir) = temp_store("corrupt-pref");
        let session = store
            .create_session_with_model("p", "t", Some("chat"), Some("plan"), None, None)
            .expect("create");

        store
            .conn
            .execute(
                "UPDATE sessions SET model_pref = ?1 WHERE id = ?2",
                rusqlite::params!["{ 这不是 JSON", session.id],
            )
            .expect("写入脏数据");

        let read = store.get_session(&session.id).expect("读取不得失败");
        assert!(read.model_pref.is_none(), "脏数据按未设置处理");
        assert_eq!(store.list_sessions().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 会话级改动记录：按会话列出、按 stream 去重，且不串到别的会话
    #[test]
    fn session_snapshots_are_listed_per_session() {
        let (store, dir) = temp_store("session-snapshots");
        let session = store
            .create_session_with_model("p", "t", Some("task"), Some("work"), None, None)
            .expect("create");
        let other = store
            .create_session_with_model("p", "t2", Some("task"), Some("work"), None, None)
            .expect("create");

        let row = |stream: &str, path: &str, at: &str| SnapshotRow {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.id.clone(),
            stream_id: stream.to_string(),
            root_id: "default".to_string(),
            rel_path: path.to_string(),
            backup_name: "backup".to_string(),
            bytes: 10,
            created_at: at.to_string(),
        };
        store.record_snapshot(&row("s1", "a.rs", "2026-01-01T00:00:00Z")).unwrap();
        store.record_snapshot(&row("s1", "b.rs", "2026-01-01T00:00:01Z")).unwrap();
        store.record_snapshot(&row("s2", "c.rs", "2026-01-02T00:00:00Z")).unwrap();
        let mut foreign = row("s3", "d.rs", "2026-01-03T00:00:00Z");
        foreign.session_id = other.id.clone();
        store.record_snapshot(&foreign).unwrap();

        let rows = store.list_session_snapshots(&session.id).unwrap();
        assert_eq!(rows.len(), 3, "只应返回本会话的记录");
        let streams = store.list_snapshot_streams(&session.id).unwrap();
        let mut sorted = streams.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["s1", "s2"], "stream 必须去重");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 会话授权：幂等、可撤销、按会话隔离，删除会话时级联清理
    #[test]
    fn session_grants_are_persistent_and_revocable() {
        let (store, dir) = temp_store("session-grants");
        let session = store
            .create_session_with_model("p", "t", Some("task"), Some("work"), None, None)
            .expect("create");
        let other = store
            .create_session_with_model("p", "t2", Some("task"), Some("work"), None, None)
            .expect("create");

        store.grant_session_tool(&session.id, "write_file").unwrap();
        store.grant_session_tool(&session.id, "write_file").unwrap(); // 幂等
        store.grant_session_tool(&session.id, "run_command").unwrap();
        store.grant_session_tool(&other.id, "web_fetch").unwrap();

        assert_eq!(
            store.list_session_grants(&session.id).unwrap(),
            vec!["run_command".to_string(), "write_file".to_string()],
            "按字母序且不重复"
        );
        assert_eq!(store.list_session_grants(&other.id).unwrap().len(), 1);

        assert!(store.revoke_session_tool(&session.id, "write_file").unwrap());
        assert!(!store.revoke_session_tool(&session.id, "write_file").unwrap());
        assert_eq!(store.list_session_grants(&session.id).unwrap(), vec!["run_command"]);

        // 删除会话 → 级联清理（外键开启时）
        store.delete_session(&session.id).unwrap();
        assert!(store.list_session_grants(&session.id).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 会话级 token 用量：消息 token 与工具隐藏开销分开累计
    #[test]
    fn session_token_usage_sums_messages_and_hidden_costs() {
        let (store, dir) = temp_store("session-usage");
        let session = store
            .create_session_with_model("p", "t", Some("task"), Some("work"), None, None)
            .expect("create");

        store
            .add_message(&session.id, Role::User, "hi", 10, 0, None, None)
            .unwrap();
        store
            .add_message(&session.id, Role::Assistant, "hello", 90, 5, None, None)
            .unwrap();

        // 一次带隐藏开销的子代理调用
        let row = ToolInvocationRow {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: session.id.clone(),
            message_id: None,
            stream_id: "s1".to_string(),
            step: 1,
            tool_name: "spawn_subagents".to_string(),
            tool_label: "子代理".to_string(),
            arguments_json: "{}".to_string(),
            status: "ok".to_string(),
            result_preview: None,
            error: None,
            truncated: false,
            duration_ms: 100,
            approval: Some("auto".to_string()),
            extra_tokens: 500,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        store.record_tool_invocations(&[row]).unwrap();

        let (messages, extra) = store.session_token_usage(&session.id).unwrap();
        assert_eq!(messages, 100);
        assert_eq!(extra, 500);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 回退：删除目标及之后的消息，级联清理工具轨迹，保留此前历史
    #[test]
    fn rewind_removes_tail_and_attached_tool_invocations() {
        let (store, dir) = temp_store("rewind-tail");
        let session = store.create_session("p", "t", None, None, None).expect("create");

        let mut ids = Vec::new();
        for (i, role) in [Role::User, Role::Assistant, Role::User, Role::Assistant]
            .into_iter()
            .enumerate()
        {
            ids.push(
                store
                    .add_message(&session.id, role, &format!("m{}", i), 1, 0, None, None)
                    .unwrap()
                    .id,
            );
        }

        // 尾部回复挂着一条工具轨迹：回退到第二条（含）后必须一起清理
        store
            .record_tool_invocations(&[ToolInvocationRow {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: session.id.clone(),
                message_id: Some(ids[3].clone()),
                stream_id: "s2".to_string(),
                step: 1,
                tool_name: "read_file".to_string(),
                tool_label: "读取文件".to_string(),
                arguments_json: "{}".to_string(),
                status: "ok".to_string(),
                result_preview: None,
                error: None,
                truncated: false,
                duration_ms: 1,
                approval: None,
                extra_tokens: 0,
                created_at: Utc::now().to_rfc3339(),
            }])
            .unwrap();

        let outcome = store.rewind_messages(&session.id, &ids[2], true).unwrap();
        assert_eq!(outcome.removed, 2);
        assert_eq!(outcome.affected_streams, vec!["s2".to_string()]);
        assert_eq!(store.get_messages(&session.id).unwrap().len(), 2);
        assert!(store.get_tool_invocations(&session.id).unwrap().is_empty());

        // 保留式截断（重试用户提问）：目标行本身留在历史里
        let session2 = store.create_session("p", "t2", None, None, None).unwrap();
        let u = store
            .add_message(&session2.id, Role::User, "hi", 1, 0, None, None)
            .unwrap();
        store
            .add_message(&session2.id, Role::Assistant, "yo", 1, 0, None, None)
            .unwrap();
        let outcome = store.rewind_messages(&session2.id, &u.id, false).unwrap();
        assert_eq!(outcome.removed, 1);
        assert_eq!(store.get_messages(&session2.id).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 回退/编辑落在已摘要区间 → 摘要必须清空；否则水位不受影响
    #[test]
    fn rewind_reconciles_summary_watermark() {
        let (store, dir) = temp_store("rewind-summary");

        let make_session = |store: &ChatStore, title: &str| {
            let session = store.create_session("p", title, None, None, None).unwrap();
            let mut ids = Vec::new();
            for i in 0..6 {
                let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
                ids.push(
                    store
                        .add_message(&session.id, role, &format!("m{}", i), 1, 0, None, None)
                        .unwrap()
                        .id,
                );
            }
            (session, ids)
        };

        // index=4 >= 水位 4：摘要覆盖的是更早的消息，无需清空
        let (session, ids) = make_session(&store, "t1");
        store.set_session_summary(&session.id, "旧摘要", 4).unwrap();
        store.rewind_messages(&session.id, &ids[4], true).unwrap();
        let (summary, count) = store.get_summary_with_count(&session.id).unwrap();
        assert_eq!(summary, "旧摘要");
        assert_eq!(count, 4);

        // index=2 < 水位 4：摘要引用了被删内容，必须整体清空重算
        let (session2, ids2) = make_session(&store, "t2");
        store.set_session_summary(&session2.id, "旧摘要", 4).unwrap();
        let outcome = store.rewind_messages(&session2.id, &ids2[2], true).unwrap();
        assert!(outcome.summary_cleared);
        let (summary, count) = store.get_summary_with_count(&session2.id).unwrap();
        assert!(summary.is_empty() && count == 0);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 回退不仅截断消息，还会清理截断点之后的工作记忆，并把计划里的
    /// "进行中"项置为"受阻"（否则下一轮会拿着已删除历史的结论继续干活）
    #[test]
    fn rewind_clears_notes_and_blocks_plan_doing() {
        use crate::agent::notes::SessionNote;
        use crate::agent::plan::{PlanItem, PlanStatus, SessionPlan};

        let (store, dir) = temp_store("rewind-state");
        let session = store.create_session("p", "t", None, None, None).unwrap();

        // 更早一轮留下的笔记：截断到 a1 时必须保留
        store
            .save_note(
                &session.id,
                &SessionNote::new("old".to_string(), None, "更早的结论".to_string()),
            )
            .unwrap();
        let u1 = store
            .add_message(&session.id, Role::User, "旧问题", 1, 0, None, None)
            .unwrap();
        // 这一轮生成期间（assistant 消息落库之前）写下的笔记：属于被删区间
        store
            .save_note(
                &session.id,
                &SessionNote::new("during".to_string(), None, "基于旧回答的结论".to_string()),
            )
            .unwrap();
        let a1 = store
            .add_message(&session.id, Role::Assistant, "旧回答", 1, 0, None, None)
            .unwrap();
        store
            .add_message(&session.id, Role::User, "接着问", 1, 0, None, None)
            .unwrap();

        store
            .save_plan(
                &session.id,
                &SessionPlan::new(
                    vec![
                        PlanItem {
                            title: "读代码".to_string(),
                            status: PlanStatus::Done,
                        },
                        PlanItem {
                            title: "改实现".to_string(),
                            status: PlanStatus::Doing,
                        },
                    ],
                    None,
                ),
            )
            .unwrap();
        store
            .save_note(
                &session.id,
                &SessionNote::new(
                    "n1".to_string(),
                    None,
                    "基于旧回答的结论".to_string(),
                ),
            )
            .unwrap();

        // 回退到 a1（含）：a1 之后的提问与作答都被删除
        let outcome = store.rewind_messages(&session.id, &a1.id, true).unwrap();
        assert_eq!(outcome.removed, 2);
        assert_eq!(
            outcome.notes_removed, 2,
            "被删区间（含生成期间写下）的笔记必须一并清理"
        );
        assert!(outcome.plan_blocked, "进行中的计划项必须被置为受阻");
        let notes = store.list_notes(&session.id).unwrap();
        assert_eq!(notes.len(), 1, "更早一轮的笔记必须保留：{notes:?}");
        assert_eq!(notes[0].id, "old");

        let plan = store.get_plan(&session.id).unwrap().expect("计划仍在");
        assert_eq!(plan.items[0].status, PlanStatus::Done, "已完成项不受影响");
        assert_eq!(plan.items[1].status, PlanStatus::Blocked, "进行中 → 受阻");

        // 回退到更早的 u1（不含）：a1 已删，目标就是第一条，无事发生
        let outcome = store.rewind_messages(&session.id, &u1.id, false).unwrap();
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.notes_removed, 0);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 编辑用户消息：正文与 token 更新、其后截断；assistant 消息不可编辑
    #[test]
    fn edit_user_message_updates_and_truncates() {
        let (store, dir) = temp_store("edit-msg");
        let session = store.create_session("p", "t", None, None, None).unwrap();
        let u1 = store
            .add_message(&session.id, Role::User, "旧问题", 3, 0, None, None)
            .unwrap();
        store
            .add_message(&session.id, Role::Assistant, "旧回答", 3, 0, None, None)
            .unwrap();
        store
            .add_message(&session.id, Role::User, "下一个问题", 3, 0, None, None)
            .unwrap();

        let outcome = store
            .edit_user_message(&session.id, &u1.id, "新问题", 2)
            .unwrap();
        assert_eq!(outcome.removed, 2, "编辑后旧回复与新提问都被截断");
        let messages = store.get_messages(&session.id).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "新问题");
        assert_eq!(messages[0].token_count, 2);

        let a = store
            .add_message(&session.id, Role::Assistant, "回答", 1, 0, None, None)
            .unwrap();
        assert!(store.edit_user_message(&session.id, &a.id, "x", 1).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }
}
