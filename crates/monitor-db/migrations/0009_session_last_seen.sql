-- Idle timeout (D-016): a session also ends when it goes unused for SMON_SESSION_IDLE_S.
-- Use refreshes `last_seen_at` (at most once a minute); NULL means "since created_at".
ALTER TABLE sessions ADD COLUMN last_seen_at TEXT;
