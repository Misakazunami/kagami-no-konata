-- 记录生成该条回复的模型（会话级模型选择 / 自动选择下主/子模型不同，
-- 历史消息也要能显示"这条是谁答的"）
ALTER TABLE messages ADD COLUMN model TEXT DEFAULT NULL;
