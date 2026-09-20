-- 工具调用的"隐藏开销"：子代理等工具自己声明的额外 token 估算
--
-- 之前这个数字只随 `message-stats` 一闪而过；不落库就无法在会话层面
-- 汇总"整个任务到底花了多少"，用户会以为任务便宜而反复派子代理。
ALTER TABLE tool_invocations ADD COLUMN extra_tokens INTEGER NOT NULL DEFAULT 0;
