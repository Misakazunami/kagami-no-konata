-- 工具调用轨迹
--
-- 定位：**只服务于 UI 回放**。工具结果从不写进 messages 表，也不进入
-- 摘要与记忆提取链路（那会让外部文件/网页内容固化成"对话历史"）。
-- 本表只保留结果预览，永不回灌给模型。
CREATE TABLE IF NOT EXISTS tool_invocations (
    id             TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL,
    -- 关联的 assistant 消息 id（消息落库后回填，可空）
    message_id     TEXT,
    stream_id      TEXT NOT NULL,
    step           INTEGER NOT NULL DEFAULT 0,
    tool_name      TEXT NOT NULL,
    tool_label     TEXT NOT NULL DEFAULT '',
    arguments_json TEXT NOT NULL DEFAULT '{}',
    -- ok | error | denied | cancelled | timeout
    status         TEXT NOT NULL,
    result_preview TEXT,
    error          TEXT,
    truncated      INTEGER NOT NULL DEFAULT 0,
    duration_ms    INTEGER NOT NULL DEFAULT 0,
    -- auto | allow_once | allow_session | deny
    approval       TEXT,
    created_at     TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_tool_invocations_session
    ON tool_invocations(session_id, created_at);
CREATE INDEX IF NOT EXISTS idx_tool_invocations_stream
    ON tool_invocations(stream_id);
