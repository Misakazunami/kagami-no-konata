-- 工作区改动快照
--
-- 定位：写类工具（write_file / edit_file / delete_path / move_path / copy_path）
-- 在**动手之前**把被覆盖/被删除/被移动的文件原样备份到
-- `{app_data_dir}/snapshots/{stream_id}/`，并把索引记在这里，
-- 让用户能一键回滚"这一次生成造成的全部文件改动"。
--
-- 只记**文件**（目录树太大，删除类操作另外走回收站兜底）；备份文件名与
-- 工作区相对路径分开存，恢复时重新拼回原位置。
CREATE TABLE IF NOT EXISTS workspace_snapshots (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL,
    stream_id   TEXT NOT NULL,
    -- 工作区 id 与相对该根的路径（恢复时用它拼回原位置）
    root_id     TEXT NOT NULL,
    rel_path    TEXT NOT NULL,
    -- 备份文件在 snapshots/{stream_id}/ 下的文件名
    backup_name TEXT NOT NULL,
    bytes       INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_snapshots_stream
    ON workspace_snapshots(stream_id);
CREATE INDEX IF NOT EXISTS idx_snapshots_session
    ON workspace_snapshots(session_id, created_at);
