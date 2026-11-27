-- An account's requests are served from its own workspace (`accounts.workspace_id`)
-- when they name none, so the account needs a seat there. Removing an account from its
-- own workspace is refused now, but earlier versions allowed it once a second owner
-- existed, and such an account is then refused everywhere: it cannot even list the
-- workspaces it still belongs to. Any account in that state that holds a seat somewhere
-- is pointed at the workspace it joined first. An account with no seat at all is left
-- as it is: there is nowhere to point it.
UPDATE accounts
   SET workspace_id = (SELECT m.workspace_id FROM members m
                        WHERE m.account_id = accounts.id
                        ORDER BY m.created_at, m.workspace_id LIMIT 1)
 WHERE NOT EXISTS (SELECT 1 FROM members m
                    WHERE m.account_id = accounts.id
                      AND m.workspace_id = accounts.workspace_id)
   AND EXISTS (SELECT 1 FROM members m WHERE m.account_id = accounts.id);
