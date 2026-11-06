-- Dashboard sessions (M7.2): server-side session rows behind the HttpOnly
-- `smon_session` cookie. The store is the authority — sessions survive restarts and
-- expire on their own timestamp, never on a client claim.
CREATE TABLE sessions (
    id         TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

CREATE INDEX sessions_expires_idx ON sessions (expires_at);
