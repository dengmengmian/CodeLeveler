-- Targeted "latest event of one type" lookups (turn seeding, context-snapshot
-- restore) query by (session, type) and want the newest row. Without this
-- index every such lookup scans and deserializes the whole session log —
-- O(N) per turn start, O(N²) over a long session.
CREATE INDEX idx_events_session_type_sequence ON events(session_id, type, sequence);
