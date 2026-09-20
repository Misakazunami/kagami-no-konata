-- 会话级持久授权：用户在审批弹窗点击「本会话允许」后，之后每次生成都免审批
--
-- 为什么需要单独一张表：一次生成内的内存授权（runner 的 `session_grants`）
-- 随生成结束即失效，而按钮文案承诺的是"本会话"。跨轮、跨窗口的语义必须落库。
-- 删除会话时级联清理；用户可在任务状态条上撤销。
CREATE TABLE IF NOT EXISTS session_grants (
    session_id TEXT NOT NULL,
    tool       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (session_id, tool),
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_session_grants_session ON session_grants(session_id);
