-- 会话级 AUTO 开关：任务会话内自动允许所有需要审批的工具调用
--
-- 只跳过审批弹窗，不改变工具可见性；命令硬黑名单、参数审查、
-- 路径监狱、敏感文件清单与写前快照全部照旧（见 harness::command_guard / jail）。
-- 按会话持久化：切换会话各自独立，普通聊天会话不允许开启。
ALTER TABLE sessions ADD COLUMN auto_approve_all INTEGER NOT NULL DEFAULT 0;
