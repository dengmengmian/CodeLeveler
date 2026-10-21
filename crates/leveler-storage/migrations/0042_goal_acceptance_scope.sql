-- Acceptance scope is part of the goal contract, not a per-turn hint.
--
-- `acceptance` is the model's candidate plan, declared once at `commit_task`
-- (or empty for a `/goal` turn). It is persisted so a resumed or compacted
-- window cannot silently lose it; there is deliberately no model-facing update
-- path, so acceptance criteria cannot be deleted mid-task.
--
-- `amendments` is the user's own later text that adjusts the task scope
-- (verbatim, one entry per user turn). It is the authority for narrowing a
-- requirement; `objective` alone is the original request.
--
-- Both are JSON arrays of strings. Legacy rows default to empty, which keeps
-- every existing goal readable and leaves its completion gate unchanged.
ALTER TABLE goals ADD COLUMN acceptance TEXT NOT NULL DEFAULT '[]';
ALTER TABLE goals ADD COLUMN amendments TEXT NOT NULL DEFAULT '[]';
