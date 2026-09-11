-- 性能优化迁移：会话摘要持久化 / 记忆提取水位 / 向量 BLOB

-- 会话上下文摘要（增量维护，替代每次全量重算）
ALTER TABLE sessions ADD COLUMN context_summary TEXT DEFAULT NULL;
ALTER TABLE sessions ADD COLUMN summarized_count INTEGER NOT NULL DEFAULT 0;

-- 记忆提取水位线（只提取上次之后新增的用户消息）
ALTER TABLE sessions ADD COLUMN last_extracted_at TEXT DEFAULT NULL;

-- 归一化后的嵌入向量（f32 小端序列化，替代逐行 JSON 解析）
ALTER TABLE memories ADD COLUMN embedding_blob BLOB DEFAULT NULL;
