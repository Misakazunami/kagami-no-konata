-- 使用统计表
CREATE TABLE IF NOT EXISTS usage_stats (
    id              INTEGER PRIMARY KEY DEFAULT 1,
    total_requests  INTEGER NOT NULL DEFAULT 0,
    total_tokens    INTEGER NOT NULL DEFAULT 0,
    prompt_tokens   INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_time_ms   INTEGER NOT NULL DEFAULT 0,
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

-- 插入默认行
INSERT OR IGNORE INTO usage_stats (id) VALUES (1);

-- 消息元数据列（token 用量 + 思考时间）
ALTER TABLE messages ADD COLUMN token_count INTEGER DEFAULT 0;
ALTER TABLE messages ADD COLUMN thinking_ms INTEGER DEFAULT 0;
