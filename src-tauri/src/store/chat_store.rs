use anyhow::Result;
use chrono::{Local, Utc};
use rusqlite::Connection;
use uuid::Uuid;

use crate::agent::context::{Message, Role, Session};

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
    })
}

const MESSAGE_COLUMNS: &str =
    "id, session_id, role, content, created_at, token_count, thinking_ms, thinking";

impl ChatStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// 创建新会话
    pub fn create_session(&self, persona_id: &str, title: &str) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        self.conn.execute(
            "INSERT INTO sessions (id, title, persona_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, title, persona_id, now, now],
        )?;

        Ok(Session {
            id,
            title: title.to_string(),
            persona_id: persona_id.to_string(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// 添加消息（带元数据，事务保证消息与会话更新原子性）
    pub fn add_message(
        &self,
        session_id: &str,
        role: Role,
        content: &str,
        token_count: i64,
        thinking_ms: i64,
        thinking: Option<String>,
    ) -> Result<Message> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        let date_key = now.format("%Y-%m-%d").to_string();
        let timestamp = now.to_rfc3339();

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO messages (id, session_id, role, content, created_at, date_key, token_count, thinking_ms, thinking) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![id, session_id, role.to_string(), content, timestamp, date_key, token_count, thinking_ms, thinking],
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
        let mut stmt = self.conn.prepare(
            "SELECT id, title, persona_id, created_at, updated_at FROM sessions ORDER BY updated_at DESC",
        )?;

        let sessions = stmt
            .query_map([], |row| {
                Ok(Session {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    persona_id: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(sessions)
    }

    /// 获取单个会话信息
    pub fn get_session(&self, session_id: &str) -> Result<Session> {
        self.conn.query_row(
            "SELECT id, title, persona_id, created_at, updated_at FROM sessions WHERE id = ?1",
            rusqlite::params![session_id],
            |row| {
                Ok(Session {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    persona_id: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        ).map_err(|e| e.into())
    }

    /// 按日期查找会话
    pub fn find_session_by_date(&self, date: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, persona_id, created_at, updated_at FROM sessions WHERE date(created_at) = ?1 ORDER BY updated_at DESC LIMIT 1",
        )?;

        let mut rows = stmt.query_map(rusqlite::params![date], |row| {
            Ok(Session {
                id: row.get(0)?,
                title: row.get(1)?,
                persona_id: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;

        match rows.next() {
            Some(session) => Ok(Some(session?)),
            None => Ok(None),
        }
    }

    /// 查找今日的空会话（无消息）
    pub fn find_empty_session_today(&self) -> Result<Option<Session>> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.title, s.persona_id, s.created_at, s.updated_at
             FROM sessions s
             LEFT JOIN messages m ON s.id = m.session_id
             WHERE date(s.created_at) = ?1
             GROUP BY s.id
             HAVING COUNT(m.id) = 0
             ORDER BY s.created_at DESC
             LIMIT 1",
        )?;

        let mut rows = stmt.query_map(rusqlite::params![today], |row| {
            Ok(Session {
                id: row.get(0)?,
                title: row.get(1)?,
                persona_id: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;

        match rows.next() {
            Some(session) => Ok(Some(session?)),
            None => Ok(None),
        }
    }

    /// 查找最近的空会话（无消息，不限日期）
    pub fn find_latest_empty_session(&self) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.title, s.persona_id, s.created_at, s.updated_at
             FROM sessions s
             LEFT JOIN messages m ON s.id = m.session_id
             GROUP BY s.id
             HAVING COUNT(m.id) = 0
             ORDER BY s.created_at DESC
             LIMIT 1",
        )?;

        let mut rows = stmt.query_map([], |row| {
            Ok(Session {
                id: row.get(0)?,
                title: row.get(1)?,
                persona_id: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;

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
}
