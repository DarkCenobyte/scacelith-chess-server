//! Login sessions (their token hashes; the tokens themselves are never stored).

use rusqlite::{Row, params};

use super::db::Db;
use super::error::Result;
use crate::ids::UserId;

/// A new session.
#[derive(Debug, Clone, Default)]
pub struct NewSession {
    pub user_id: UserId,
    /// SHA-256 of the session token (lowercase hex).
    pub token_hash: String,
    pub created_at: i64,
    /// Absolute expiry.
    pub expires_at: i64,
    /// Idle expiry (`None`: the absolute expiry).
    pub idle_expires_at: Option<i64>,
    pub client_label: Option<String>,
    pub ip: Option<String>,
}

/// A session found by its token hash (no liveness filter: the caller checks the times).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAuth {
    pub id: i64,
    pub user_id: UserId,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub idle_expires_at: i64,
    pub revoked_at: Option<i64>,
}

/// A session as listed to its owner (never with its token hash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: i64,
    pub created_at: i64,
    pub last_seen_at: i64,
    pub expires_at: i64,
    pub idle_expires_at: i64,
    /// `None` in [`Sessions::list_for_user`], which lists only the non-revoked sessions.
    pub revoked_at: Option<i64>,
    pub client_label: Option<String>,
    pub ip: Option<String>,
}

fn to_info(r: &Row<'_>) -> rusqlite::Result<SessionInfo> {
    Ok(SessionInfo {
        id: r.get(0)?,
        created_at: r.get(1)?,
        last_seen_at: r.get(2)?,
        expires_at: r.get(3)?,
        idle_expires_at: r.get(4)?,
        revoked_at: r.get(5)?,
        client_label: r.get(6)?,
        ip: r.get(7)?,
    })
}

/// The sessions table.
#[derive(Debug, Clone, Copy)]
pub struct Sessions<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Sessions<'_> {
    /// Creates a session; returns its id. Error `duplicate` for a token hash already stored.
    pub fn create(&self, s: &NewSession) -> Result<i64> {
        self.db.insert(
            "INSERT INTO sessions (user_id, token_hash, created_at, last_seen_at, expires_at, idle_expires_at,
             client_label, ip) VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7)",
            params![
                s.user_id,
                s.token_hash,
                s.created_at,
                s.expires_at,
                s.idle_expires_at.unwrap_or(s.expires_at),
                s.client_label,
                s.ip,
            ],
        )
    }

    /// The session of a token hash, revoked or expired included.
    pub fn by_token_hash(&self, hash: &str) -> Result<Option<SessionAuth>> {
        self.db.one(
            "SELECT id, user_id, created_at, last_seen_at, expires_at, idle_expires_at, revoked_at
             FROM sessions WHERE token_hash = ?1",
            [hash],
            |r| {
                Ok(SessionAuth {
                    id: r.get(0)?,
                    user_id: r.get(1)?,
                    created_at: r.get(2)?,
                    last_seen_at: r.get(3)?,
                    expires_at: r.get(4)?,
                    idle_expires_at: r.get(5)?,
                    revoked_at: r.get(6)?,
                })
            },
        )
    }

    /// Records activity: last seen now, new idle expiry.
    pub fn touch(&self, id: i64, now: i64, idle_expires_at: i64) -> Result<()> {
        self.db.exec(
            "UPDATE sessions SET last_seen_at = ?1, idle_expires_at = ?2 WHERE id = ?3",
            params![now, idle_expires_at, id],
        )?;
        Ok(())
    }

    /// Revokes a live session (owned by `user_id` when given); returns its token hash, `None`
    /// when there was nothing to revoke.
    pub fn revoke(&self, id: i64, user_id: Option<UserId>, now: i64) -> Result<Option<String>> {
        match user_id {
            None => self.db.one(
                "UPDATE sessions SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL RETURNING token_hash",
                params![now, id],
                |r| r.get(0),
            ),
            Some(user) => self.db.one(
                "UPDATE sessions SET revoked_at = ?1 WHERE id = ?2 AND user_id = ?3 AND revoked_at IS NULL
                 RETURNING token_hash",
                params![now, id, user],
                |r| r.get(0),
            ),
        }
    }

    /// Revokes every live session of the account but `except`; returns their token hashes.
    pub fn revoke_all_for_user(&self, user_id: UserId, except: Option<i64>, now: i64) -> Result<Vec<String>> {
        self.db.all(
            "UPDATE sessions SET revoked_at = ?1 WHERE user_id = ?2 AND revoked_at IS NULL AND id IS NOT ?3
             RETURNING token_hash",
            params![now, user_id, except],
            |r| r.get(0),
        )
    }

    /// The non-revoked sessions of the account (expired ones included), most recently seen first.
    pub fn list_for_user(&self, user_id: UserId) -> Result<Vec<SessionInfo>> {
        self.db.all(
            "SELECT id, created_at, last_seen_at, expires_at, idle_expires_at, revoked_at, client_label, ip
             FROM sessions WHERE user_id = ?1 AND revoked_at IS NULL ORDER BY last_seen_at DESC, id DESC",
            [user_id],
            to_info,
        )
    }

    /// Every stored session of the account, revoked and expired ones included (until the retention
    /// deletes them), newest first.
    pub fn all_for_user(&self, user_id: UserId) -> Result<Vec<SessionInfo>> {
        self.db.all(
            "SELECT id, created_at, last_seen_at, expires_at, idle_expires_at, revoked_at, client_label, ip
             FROM sessions WHERE user_id = ?1 ORDER BY created_at DESC, id DESC",
            [user_id],
            to_info,
        )
    }

    /// Keeps the `max` newest live sessions of the account and revokes the others; returns the
    /// token hashes revoked, newest first.
    pub fn enforce_limit(&self, user_id: UserId, max: i64, now: i64) -> Result<Vec<String>> {
        self.db.transaction(|db| {
            let live: Vec<(i64, String)> = db.all(
                "SELECT id, token_hash FROM sessions WHERE user_id = ?1 AND revoked_at IS NULL AND expires_at > ?2
                 AND idle_expires_at > ?2 ORDER BY created_at DESC, id DESC",
                params![user_id, now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let keep = usize::try_from(max.max(0)).unwrap_or(usize::MAX);
            let mut out = Vec::new();
            for (id, hash) in live.into_iter().skip(keep) {
                db.exec("UPDATE sessions SET revoked_at = ?1 WHERE id = ?2", params![now, id])?;
                out.push(hash);
            }
            Ok(out)
        })
    }
}
