use anyhow::Result;
use chrono::{Local, Utc};
use rusqlite::Connection;
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
const SESSION_COLUMNS: &str =
    "id, title, persona_id, session_type, task_mode, workspace_id, model_pref, created_at, updated_at";

/// 带表别名的列清单（`find_latest_empty_session` 的 JOIN 查询用）
const SESSION_COLUMNS_ALIASED: &str = "s.id, s.title, s.persona_id, s.session_type, s.task_mode, \
     s.workspace_id, s.model_pref, s.created_at, s.updated_at";

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
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
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

    /// 按日期查找会话
    pub fn find_session_by_date(&self, date: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM sessions WHERE date(created_at) = ?1 ORDER BY updated_at DESC LIMIT 1",
            SESSION_COLUMNS
        ))?;

        let mut rows = stmt.query_map(rusqlite::params![date], map_session_row)?;

        match rows.next() {
            Some(session) => Ok(Some(session?)),
            None => Ok(None),
        }
    }

    /// 查找今日的空会话（无消息）
    pub fn find_empty_session_today(&self) -> Result<Option<Session>> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {}
             FROM sessions s
             LEFT JOIN messages m ON s.id = m.session_id
             WHERE date(s.created_at) = ?1
             GROUP BY s.id
             HAVING COUNT(m.id) = 0
             ORDER BY s.created_at DESC
             LIMIT 1",
            SESSION_COLUMNS_ALIASED
        ))?;

        let mut rows = stmt.query_map(rusqlite::params![today], map_session_row)?;

        match rows.next() {
            Some(session) => Ok(Some(session?)),
            None => Ok(None),
        }
    }

    /// 查找最近的空会话（无消息，不限日期）
    pub fn find_latest_empty_session(&self) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {}
             FROM sessions s
             LEFT JOIN messages m ON s.id = m.session_id
             GROUP BY s.id
             HAVING COUNT(m.id) = 0
             ORDER BY s.created_at DESC
             LIMIT 1",
            SESSION_COLUMNS_ALIASED
        ))?;

        let mut rows = stmt.query_map([], map_session_row)?;

        match rows.next() {
            Some(session) => Ok(Some(session?)),
            None => Ok(None),
        }
    }

    /// 更新会话标题
    pub fn update_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET title = ?1 WHERE id = ?2",
            rusqlite::params![title, session_id],
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
                     approval, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                    approval, created_at
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
                    created_at: row.get(14)?,
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
    /// 再按**总字节**（最多 `MAX_TOTAL_BYTES`）。淘汰只发生在写的时候，
    /// 因此读路径永远是"取出来就能直接注入"，不需要在提示词组装阶段做裁剪。
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
        tx.commit()?;
        self.trim_notes(session_id)?;
        Ok(())
    }

    /// 按条数与总量上限淘汰超出部分（最旧的先走）
    pub fn trim_notes(&self, session_id: &str) -> Result<()> {
        let notes = self.list_notes(session_id)?;
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
                self.conn.execute(
                    "DELETE FROM tool_notes WHERE id = ?1",
                    [&note.id],
                )?;
            }
        }
        Ok(())
    }

    /// 读取某个会话的工作记忆（按时间正序，最旧的在前）
    pub fn list_notes(&self, session_id: &str) -> Result<Vec<SessionNote>> {
        let mut stmt = self.conn.prepare(
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
            .find_latest_empty_session()
            .expect("query ok")
            .expect("应当找到空会话");
        assert_eq!(found.id, session.id);
        assert_eq!(found.model_pref.as_ref(), Some(&pref));
        assert_eq!(found.session_type, "task", "其他列也不能错位");

        // `created_at` 存的是 RFC3339 的 UTC 时间，这一列的比较也必须用 UTC 日期
        let today = Utc::now().format("%Y-%m-%d").to_string();
        assert!(store.find_session_by_date(&today).unwrap().is_some());
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
}
