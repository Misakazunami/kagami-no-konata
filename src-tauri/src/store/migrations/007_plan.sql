-- 会话级任务计划
--
-- 定位：模型通过 update_plan 工具维护的**结构化进度**，每次生成注入 system prompt，
-- 同时通过 plan-updated 事件在界面上展示同一份内容。
--
-- 一条会话一行，整体存 JSON：计划项很少（≤20），拆成表反而要处理排序与增量更新，
-- 而这里永远是"整份覆盖"，所以 JSON 列最简单也最不容易出现半更新状态。
CREATE TABLE IF NOT EXISTS session_plans (
    session_id TEXT PRIMARY KEY,
    plan_json  TEXT NOT NULL DEFAULT '{"items":[]}',
    updated_at TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);
