-- 会话级模型选择（手动选择 / 自动选择）与深度思考开关
--
-- 单个 JSON 列而不是多个列：这些字段从不参与查询与排序，
-- 存成 JSON 后以后加字段只需 serde default，不必再写迁移。
-- 形状见 `llm::router::SessionModelPref`：
--   {"mode":"inherit|manual|auto","provider_id":null,"model":null,"thinking":null}
ALTER TABLE sessions ADD COLUMN model_pref TEXT DEFAULT NULL;
