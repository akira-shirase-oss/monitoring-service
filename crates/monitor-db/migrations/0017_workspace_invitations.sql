-- S2 review (S2-4): a seat is earned by the invitee's acceptance, never seated by
-- the inviter's say-so. An invitation carries the email it was sent to and the role
-- it will grant; `members` gains a row only when an account whose email matches
-- accepts (or when the operator seats someone directly, which stays an operator
-- act). One pending invitation per (workspace, email) — re-inviting refreshes it.
CREATE TABLE IF NOT EXISTS workspace_invitations (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    email        TEXT NOT NULL,
    role         TEXT NOT NULL CHECK (role IN ('member', 'viewer')),
    invited_by   TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    UNIQUE (workspace_id, email)
);
-- The invitee's lookup: every invitation addressed to their account's email.
CREATE INDEX IF NOT EXISTS workspace_invitations_email ON workspace_invitations (email);
