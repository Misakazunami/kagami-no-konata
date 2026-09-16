-- 会话级"工作记忆"（模型主动记下的跨轮结论）
--
-- 定位：工具结果刻意不跨轮保留，长任务因此每轮都要重读同样的文件。
-- 本表只存**模型主动调用 save_note 写下的结论**（不是工具原始输出），
-- 每条都有长度上限、每会话有总量上限，注入时统一带 untrusted 标记。
--
-- 与 tool_invocations 的区别：那张表只服务 UI 回放、永不回灌给模型；
-- 这张表会被注入 system prompt，所以边界必须更严（有上限、可清空、可审计）。
CREATE TABLE IF NOT EXISTS tool_notes (
    id         TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    title      TEXT,
    content    TEXT NOT NULL,
    bytes      INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_tool_notes_session
    ON tool_notes(session_id, created_at);
