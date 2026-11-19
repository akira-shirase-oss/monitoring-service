-- S1b (D-020): accounts behind invite-gated signup. The invite is shown once and kept as
-- a digest; a single atomic consume marks it used. Passwords are argon2id PHC strings.
-- Sessions gain an optional account owner so an account cookie resolves its workspace;
-- NULL stays an operator session.
CREATE TABLE accounts (
    id            TEXT PRIMARY KEY,           -- uuid
    workspace_id  TEXT NOT NULL REFERENCES workspaces(id),
    email         TEXT NOT NULL UNIQUE,       -- stored normalized (lowercase, trimmed)
    password_hash TEXT NOT NULL,              -- argon2id PHC string
    created_at    TEXT NOT NULL
);

CREATE INDEX accounts_workspace_idx ON accounts (workspace_id);

CREATE TABLE invites (
    id          TEXT PRIMARY KEY,             -- uuid, not the secret
    code_digest TEXT NOT NULL UNIQUE,         -- SHA-256("smon-invite-v1\0" ‖ code)
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,                -- invites die on their own timestamp
    used_at     TEXT,                         -- NULL until one signup consumes it
    used_by     TEXT REFERENCES accounts(id)
);

ALTER TABLE sessions ADD COLUMN account_id TEXT REFERENCES accounts(id);
