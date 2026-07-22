-- 归档会话：archived_at 非空即从默认会话列表隐藏。独立于 status
-- （status 是运行位置,归档是用户整理动作,两者正交）。
ALTER TABLE sessions ADD COLUMN archived_at TEXT;
