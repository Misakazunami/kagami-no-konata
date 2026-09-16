-- 会话类型与任务模式支持
-- session_type: 'chat' | 'task'
-- task_mode: 'plan' | 'work'
-- workspace_id: 指定工作区 ID（可为 NULL，默认使用 default 工作区）
ALTER TABLE sessions ADD COLUMN session_type TEXT NOT NULL DEFAULT 'chat';
ALTER TABLE sessions ADD COLUMN task_mode TEXT NOT NULL DEFAULT 'plan';
ALTER TABLE sessions ADD COLUMN workspace_id TEXT DEFAULT NULL;
