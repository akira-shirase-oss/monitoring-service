//! Account lifecycle in the store (D-027): the credential a password change or an
//! account deletion confirms, the password change itself, the deletion of an account
//! together with the workspaces only it could reach, and the bounded reads a workspace
//! export is assembled from.
//!
//! Every statement here is a static string. Nothing is assembled at run time, and each
//! delete names the workspace or the account it is bound to.

use sqlx::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

use monitor_domain::{AccountId, WorkspaceId};

use crate::{Db, MaintenanceRow, OwnerDb, QueryError};

/// One workspace's recorded data, children before parents: the foreign keys refuse any
/// other order. Each statement is bound twice to the workspace: by the row's own
/// `workspace_id` and by the monitor it belongs to, which is also what lets `SQLite`
/// reach the rows through the per-monitor indexes instead of scanning every tenant's
/// checks. The monitors go last; their maintenance windows and status-page listing
/// cascade.
const PURGE_WORKSPACE: [&str; 5] = [
    "DELETE FROM notify_outbox WHERE workspace_id = ?1
        AND monitor_id IN (SELECT id FROM monitors WHERE workspace_id = ?1)",
    "DELETE FROM incidents WHERE workspace_id = ?1
        AND monitor_id IN (SELECT id FROM monitors WHERE workspace_id = ?1)",
    "DELETE FROM checks WHERE workspace_id = ?1
        AND monitor_id IN (SELECT id FROM monitors WHERE workspace_id = ?1)",
    "DELETE FROM check_stats_daily WHERE workspace_id = ?1
        AND monitor_id IN (SELECT id FROM monitors WHERE workspace_id = ?1)",
    "DELETE FROM monitors WHERE workspace_id = ?1",
];

/// What an account leaves behind in rows that outlive it, then the account. The order
/// is the foreign keys': everything that names the account is cleared or removed
/// before the `accounts` row goes.
///
/// - Invitations addressed to its email are removed: nobody is left to accept them.
/// - The signup invite it spent stays spent (`used_at` is kept); only the pointer to
///   the account is cleared.
/// - API tokens it minted in workspaces that survive keep working: they are the
///   workspace's credentials, and its remaining owners can revoke them. Only the
///   "created by" pointer is cleared.
const FORGET_ACCOUNT: [&str; 5] = [
    "DELETE FROM workspace_invitations
      WHERE email = (SELECT email FROM accounts WHERE id = ?1)",
    "UPDATE invites SET used_by = NULL WHERE used_by = ?1",
    "UPDATE api_tokens SET created_by = NULL WHERE created_by = ?1",
    "DELETE FROM members WHERE account_id = ?1",
    "DELETE FROM accounts WHERE id = ?1",
];

/// One `check_stats_daily` row: a monitor's outcome counts for one UTC day, what is
/// left of its checks once retention has rolled the raw rows up.
#[derive(Debug, FromRow)]
pub struct DailyStatsRow {
    pub monitor_id: String,
    pub day: String,
    pub pass: i64,
    pub http_fail: i64,
    pub transport: i64,
    pub timeout: i64,
    pub missed: i64,
}

impl Db {
    /// The argon2id hash a signed-in account's password is confirmed against. Only the
    /// two lifecycle routes read it; `None` means the account no longer exists.
    pub async fn account_password_hash(
        &self,
        account: AccountId,
    ) -> Result<Option<String>, QueryError> {
        sqlx::query_scalar("SELECT password_hash FROM accounts WHERE id = ?")
            .bind(account.0.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(Into::into)
    }

    /// Store a new password hash and end every other session of the account, in one
    /// transaction: a reader sees the old password with the old sessions or the new
    /// password with only `keep_session`. `false` means the account no longer exists.
    pub async fn set_account_password(
        &self,
        account: AccountId,
        password_hash: &str,
        keep_session: &str,
    ) -> Result<bool, QueryError> {
        let id = account.0.to_string();
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query("UPDATE accounts SET password_hash = ? WHERE id = ?")
            .bind(password_hash)
            .bind(&id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if changed == 0 {
            return Ok(false);
        }
        sqlx::query("DELETE FROM sessions WHERE account_id = ? AND id != ?")
            .bind(&id)
            .bind(keep_session)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Open an account session only if `password_hash` is still the account's stored
    /// hash. Sign-in verifies a password against a hash it read a moment earlier; a
    /// password change or an account deletion that commits in between must not be
    /// followed by a session earned with the old password. `false` means the hash
    /// moved (or the account is gone) and no session was created.
    pub async fn create_account_session_if_current(
        &self,
        key: &str,
        account: AccountId,
        password_hash: &str,
        now: OffsetDateTime,
        expires_at: OffsetDateTime,
    ) -> Result<bool, QueryError> {
        let created = sqlx::query(
            "INSERT INTO sessions (id, account_id, created_at, expires_at)
             SELECT ?1, id, ?3, ?4 FROM accounts WHERE id = ?2 AND password_hash = ?5",
        )
        .bind(key)
        .bind(account.0.to_string())
        .bind(now)
        .bind(expires_at)
        .bind(password_hash)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(created == 1)
    }

    /// Delete an account (D-027), in one transaction.
    ///
    /// Refused with [`QueryError::Conflict`] while the account is the only owner of a
    /// workspace that has other members: they would be left in a workspace nobody can
    /// administer. Otherwise every workspace whose only member is this account is
    /// deleted with everything recorded in it, the account's seats in other workspaces
    /// are removed, and its sessions, pending invitations and the account row go.
    ///
    /// Also refused, the same way, while one of those workspaces holds more than
    /// `max_rows` rows of evidence (raw checks, incidents and queued alerts counted
    /// together): every workspace's observation commits wait on the
    /// store's one write lock, so the single transaction that removes a workspace must
    /// stay small. Deleting monitors with their history works in batches (D-026) and
    /// brings a workspace under the bound.
    ///
    /// Returns the workspaces that were deleted, so the caller can drop what it holds
    /// about them outside the store (the schedule, the public status pages).
    ///
    /// The seeded default workspace is never deleted: it is the operator's, whoever
    /// sits in it. The first statement is a write on purpose: it takes the store's
    /// write lock before anything is read, so the "only member" and "only owner"
    /// answers cannot change between the read and the deletes.
    pub async fn delete_account(
        &self,
        account: AccountId,
        max_rows: i64,
    ) -> Result<Vec<WorkspaceId>, QueryError> {
        let id = account.0.to_string();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM sessions WHERE account_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        let orphaned: Option<String> = sqlx::query_scalar(
            "SELECT w.name FROM members m JOIN workspaces w ON w.id = m.workspace_id
              WHERE m.account_id = ?1 AND m.role = 'owner'
                AND EXISTS (SELECT 1 FROM members o
                             WHERE o.workspace_id = m.workspace_id AND o.account_id != ?1)
                AND NOT EXISTS (SELECT 1 FROM members o
                                 WHERE o.workspace_id = m.workspace_id
                                   AND o.account_id != ?1 AND o.role = 'owner')
              ORDER BY w.name, w.id LIMIT 1",
        )
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(name) = orphaned {
            // Dropping the transaction rolls the session delete back.
            return Err(QueryError::Conflict(format!(
                "the workspace \"{name}\" needs an owner: make another member an owner, \
                 then delete your account"
            )));
        }
        let sole: Vec<String> = sqlx::query_scalar(
            "SELECT m.workspace_id FROM members m
              WHERE m.account_id = ?1 AND m.workspace_id != ?2
                AND NOT EXISTS (SELECT 1 FROM members o
                                 WHERE o.workspace_id = m.workspace_id AND o.account_id != ?1)
              ORDER BY m.workspace_id",
        )
        .bind(&id)
        .bind(WorkspaceId::DEFAULT.0.to_string())
        .fetch_all(&mut *tx)
        .await?;
        for workspace in &sole {
            // Checks, incidents and queued alerts together: an incident-heavy
            // workspace (a monitor that flaps for months) is as slow to remove as a
            // check-heavy one. Each count goes through the per-monitor index and stops
            // at the bound, so the question is cheap however much the workspace holds.
            let heavy: Option<String> = sqlx::query_scalar(
                "SELECT w.name FROM workspaces w
                  WHERE w.id = ?1
                    AND (SELECT COUNT(*) FROM (
                           SELECT 1 FROM checks c
                            WHERE c.workspace_id = ?1
                              AND c.monitor_id IN (SELECT id FROM monitors WHERE workspace_id = ?1)
                            LIMIT ?2))
                      + (SELECT COUNT(*) FROM (
                           SELECT 1 FROM incidents i
                            WHERE i.workspace_id = ?1
                              AND i.monitor_id IN (SELECT id FROM monitors WHERE workspace_id = ?1)
                            LIMIT ?2))
                      + (SELECT COUNT(*) FROM (
                           SELECT 1 FROM notify_outbox o
                            WHERE o.workspace_id = ?1
                              AND o.monitor_id IN (SELECT id FROM monitors WHERE workspace_id = ?1)
                            LIMIT ?2)) >= ?2",
            )
            .bind(workspace)
            .bind(max_rows.saturating_add(1))
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(name) = heavy {
                return Err(QueryError::Conflict(format!(
                    "the workspace \"{name}\" holds too much history to delete in one step: \
                     delete its monitors with their history, then delete your account"
                )));
            }
        }
        for workspace in &sole {
            purge_workspace(&mut tx, workspace, &id).await?;
        }
        for statement in FORGET_ACCOUNT {
            sqlx::query(statement).bind(&id).execute(&mut *tx).await?;
        }
        // Last: nothing references these rows any more. Their members, API tokens,
        // invitations, held slugs and status-page listing cascade.
        for workspace in &sole {
            sqlx::query("DELETE FROM workspaces WHERE id = ?")
                .bind(workspace)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        sole.iter()
            .map(|workspace| {
                Uuid::parse_str(workspace)
                    .map(WorkspaceId)
                    .map_err(|e| QueryError::Corrupt(format!("workspace id: {e}")))
            })
            .collect()
    }
}

/// Empty one workspace that is about to be deleted, and move any *other* account that
/// still calls it home.
///
/// `accounts.workspace_id` is where a session lands when it names no workspace, and it
/// must point at a workspace that exists. An account can no longer be removed from its
/// own workspace, but a row written before that rule can still point here without a
/// seat (with a seat, this workspace would not be deleted). Such an account is moved to
/// the workspace it joined first, or to the seeded default workspace when it has no
/// seat anywhere: there it is exactly as it was before, an account that belongs to no
/// workspace, and no seat is created for it.
async fn purge_workspace(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    workspace: &str,
    leaving: &str,
) -> Result<(), QueryError> {
    for statement in PURGE_WORKSPACE {
        sqlx::query(statement)
            .bind(workspace)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query(
        "UPDATE accounts SET workspace_id = COALESCE(
                (SELECT m.workspace_id FROM members m WHERE m.account_id = accounts.id
                  ORDER BY m.created_at, m.workspace_id LIMIT 1), ?3)
          WHERE workspace_id = ?1 AND id != ?2",
    )
    .bind(workspace)
    .bind(leaving)
    .bind(WorkspaceId::DEFAULT.0.to_string())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl OwnerDb<'_> {
    /// Every maintenance window of this workspace, newest first, at most `limit`: the
    /// export's read. Owner work, like the export itself.
    pub async fn export_windows(&self, limit: i64) -> Result<Vec<MaintenanceRow>, QueryError> {
        let view = &self.view.view.view;
        sqlx::query_as::<_, MaintenanceRow>(
            "SELECT id, monitor_id, starts_at, ends_at, reason, created_at
               FROM maintenance_windows WHERE workspace_id = ?
              ORDER BY starts_at DESC, id DESC LIMIT ?",
        )
        .bind(view.workspace.0.to_string())
        .bind(limit)
        .fetch_all(&view.db.pool)
        .await
        .map_err(Into::into)
    }

    /// This workspace's daily aggregates, newest day first, at most `limit` rows: the
    /// export's read. Reached through the workspace's monitors, so the per-monitor
    /// primary key is used and no other tenant's rows are scanned.
    pub async fn export_daily_stats(&self, limit: i64) -> Result<Vec<DailyStatsRow>, QueryError> {
        let view = &self.view.view.view;
        sqlx::query_as::<_, DailyStatsRow>(
            "SELECT monitor_id, day, pass, http_fail, transport, timeout, missed
               FROM check_stats_daily
              WHERE workspace_id = ?1
                AND monitor_id IN (SELECT id FROM monitors WHERE workspace_id = ?1)
              ORDER BY day DESC, monitor_id LIMIT ?2",
        )
        .bind(view.workspace.0.to_string())
        .bind(limit)
        .fetch_all(&view.db.pool)
        .await
        .map_err(Into::into)
    }
}
