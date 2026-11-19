-- S1c: workspace members carry roles, and workspaces carry API tokens.
-- members.role decides what an account may do there; api_tokens are bearer
-- credentials stored only as digests — the plaintext is shown once at mint and
-- never persisted. A token's scope is its workspace row; revocation is a
-- timestamp, so history (who had it, when it was last used) survives the revoke.
CREATE TABLE members (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    role         TEXT NOT NULL CHECK (role IN ('owner','member','viewer')),
    created_at   TEXT NOT NULL,
    PRIMARY KEY (workspace_id, account_id)
);

CREATE INDEX members_account_idx ON members (account_id);

-- Accounts minted before the table existed still own their home workspace —
-- without this row they'd sign in and be refused every scoped request.
INSERT INTO members (workspace_id, account_id, role, created_at)
SELECT workspace_id, id, 'owner', created_at FROM accounts;

CREATE TABLE api_tokens (
    id           TEXT PRIMARY KEY,            -- uuid
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    created_by   TEXT REFERENCES accounts(id),
    name         TEXT NOT NULL,
    token_digest TEXT NOT NULL UNIQUE,        -- SHA-256("smon-api-token-v1\0" ‖ token)
    role         TEXT NOT NULL CHECK (role IN ('member','viewer')),
    created_at   TEXT NOT NULL,
    revoked_at   TEXT,                        -- NULL until revoked; the row stays
    last_used_at TEXT                         -- batched, never per-request
);

CREATE INDEX api_tokens_workspace_idx ON api_tokens (workspace_id);
