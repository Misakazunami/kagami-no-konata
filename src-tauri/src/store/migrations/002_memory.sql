-- 记忆条目表
CREATE TABLE IF NOT EXISTS memories (
    id             TEXT PRIMARY KEY,
    content        TEXT NOT NULL,        -- 记忆内容摘要
    memory_type    TEXT NOT NULL,        -- 'fact' | 'preference' | 'experience' | 'emotional'
    importance     REAL DEFAULT 0.5,     -- 重要性权重 [0, 1]
    embedding      TEXT,                 -- JSON 序列化的 Vec<f32>
    source_session TEXT,                 -- 来源会话 ID
    created_at     TEXT NOT NULL,
    last_accessed  TEXT NOT NULL,
    access_count   INTEGER DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(memory_type);
CREATE INDEX IF NOT EXISTS idx_memories_source ON memories(source_session);
