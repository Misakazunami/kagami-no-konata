-- 会话表
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    title       TEXT NOT NULL DEFAULT '新会话',
    persona_id  TEXT NOT NULL DEFAULT 'konata-default',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- 消息表（按天分区查询优化）
CREATE TABLE IF NOT EXISTS messages (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,
    role        TEXT NOT NULL,  -- 'user' | 'assistant' | 'system'
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    date_key    TEXT NOT NULL,  -- 'YYYY-MM-DD'
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
CREATE INDEX IF NOT EXISTS idx_messages_date ON messages(date_key);
CREATE INDEX IF NOT EXISTS idx_messages_session_date ON messages(session_id, date_key);
