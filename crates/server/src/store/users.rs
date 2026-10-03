//! Accounts (`users`) and MFA recovery codes.

use rusqlite::types::Value as SqlValue;
use rusqlite::{Row, params, params_from_iter};

use super::db::Db;
use super::error::{Result, StoreError};
use super::values::{check_username, clean_email, js_trim, normalize_email, text_enum, username_lower};
use crate::ids::UserId;

text_enum! {
    /// Status of an account.
    pub enum UserStatus {
        Active = "active",
        /// Anonymized: the row stays for the games and ratings that reference it.
        Deleted = "deleted",
    }
}

/// An account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub password_hash: Option<String>,
    pub mfa_enabled: bool,
    /// The sealed TOTP secret (`v1.` + base64url), as the auth module stores it.
    pub mfa_secret_enc: Option<String>,
    /// The sealed secret of an MFA enrolment not confirmed yet.
    pub pending_mfa_secret_enc: Option<String>,
    /// Last accepted TOTP step (replay protection).
    pub mfa_last_step: i64,
    pub status: UserStatus,
    pub accept_challenges: bool,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
    pub deleted_at: Option<i64>,
}

/// A new account.
#[derive(Debug, Clone, Default)]
pub struct NewUser {
    /// 1 to 64 UTF-16 units; unique ignoring case.
    pub username: String,
    /// Stored trimmed; unique after normalization (trimmed, lower-cased).
    pub email: Option<String>,
    pub password_hash: Option<String>,
    pub email_verified: bool,
    pub accept_challenges: bool,
    pub created_at: i64,
}

/// Fields of an account to change (`None`: unchanged). For nullable columns, `Some(None)` clears.
#[derive(Debug, Clone, Default)]
pub struct UserUpdate {
    pub username: Option<String>,
    pub email: Option<Option<String>>,
    pub email_verified: Option<bool>,
    pub password_hash: Option<Option<String>>,
    pub mfa_enabled: Option<bool>,
    pub mfa_secret_enc: Option<Option<String>>,
    pub pending_mfa_secret_enc: Option<Option<String>>,
    pub mfa_last_step: Option<i64>,
    pub status: Option<UserStatus>,
    pub accept_challenges: Option<bool>,
    pub last_login_at: Option<Option<i64>>,
    pub deleted_at: Option<Option<i64>>,
}

/// What [`Users::anonymize`] revoked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Anonymized {
    /// Token hashes of the sessions that were live (for the auth caches).
    pub token_hashes: Vec<String>,
}

const USER_COLS: &str = "id, username, email, email_verified, password_hash, mfa_enabled, mfa_secret_enc, \
     mfa_pending_secret_enc, mfa_last_step, status, accept_challenges, created_at, last_login_at, deleted_at";

fn to_user(r: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        email: r.get(2)?,
        email_verified: r.get(3)?,
        password_hash: r.get(4)?,
        mfa_enabled: r.get(5)?,
        mfa_secret_enc: r.get(6)?,
        pending_mfa_secret_enc: r.get(7)?,
        mfa_last_step: r.get(8)?,
        status: r.get(9)?,
        accept_challenges: r.get(10)?,
        created_at: r.get(11)?,
        last_login_at: r.get(12)?,
        deleted_at: r.get(13)?,
    })
}

/// A rowid as a user id.
pub(crate) fn user_id(rowid: i64) -> Result<UserId> {
    UserId::try_from(rowid).map_err(|_| StoreError::invalid(format!("user id {rowid} out of range")))
}

/// The accounts table.
#[derive(Debug, Clone, Copy)]
pub struct Users<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Users<'_> {
    /// Creates an account. Errors: `username_taken`, `email_taken`, `invalid`.
    pub fn create(&self, u: &NewUser) -> Result<UserId> {
        check_username(&u.username)?;
        let email = u.email.as_deref();
        let id = self
            .db
            .insert(
                "INSERT INTO users (username, username_lower, email, email_normalized, email_verified, password_hash,
                 accept_challenges, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    u.username,
                    username_lower(&u.username),
                    email.and_then(clean_email),
                    email.and_then(normalize_email),
                    u.email_verified,
                    u.password_hash,
                    u.accept_challenges,
                    u.created_at,
                ],
            )
            .map_err(StoreError::user_clash)?;
        user_id(id)
    }

    /// The account with this id.
    pub fn by_id(&self, id: UserId) -> Result<Option<User>> {
        self.db.one(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), [id], to_user)
    }

    /// The account with this username (case-insensitive).
    pub fn by_username(&self, name: &str) -> Result<Option<User>> {
        self.db.one(
            &format!("SELECT {USER_COLS} FROM users WHERE username_lower = ?1"),
            [username_lower(name)],
            to_user,
        )
    }

    /// The account with this address (normalized first).
    pub fn by_email(&self, email: &str) -> Result<Option<User>> {
        let Some(n) = normalize_email(email) else { return Ok(None) };
        self.db.one(&format!("SELECT {USER_COLS} FROM users WHERE email_normalized = ?1"), [n], to_user)
    }

    /// The account named by a login: an address when it contains `@`, else a username (trimmed).
    pub fn by_login(&self, login: &str) -> Result<Option<User>> {
        if login.contains('@') { self.by_email(login) } else { self.by_username(js_trim(login)) }
    }

    /// Changes the given fields; `true` when the account exists. Errors: `username_taken`,
    /// `email_taken` (the unique indexes decide, atomically), `invalid`.
    pub fn update(&self, id: UserId, f: &UserUpdate) -> Result<bool> {
        let mut cols: Vec<&str> = Vec::new();
        let mut vals: Vec<SqlValue> = Vec::new();
        let mut set = |col: &'static str, v: SqlValue| {
            cols.push(col);
            vals.push(v);
        };
        let text = |v: Option<String>| v.map_or(SqlValue::Null, SqlValue::Text);
        let int = |v: Option<i64>| v.map_or(SqlValue::Null, SqlValue::Integer);
        let flag = |v: bool| SqlValue::Integer(v.into());
        if let Some(name) = &f.username {
            check_username(name)?;
            set("username", SqlValue::Text(name.clone()));
            set("username_lower", SqlValue::Text(username_lower(name)));
        }
        if let Some(email) = &f.email {
            set("email", text(email.as_deref().and_then(clean_email)));
            set("email_normalized", text(email.as_deref().and_then(normalize_email)));
        }
        if let Some(v) = f.email_verified {
            set("email_verified", flag(v));
        }
        if let Some(v) = &f.password_hash {
            set("password_hash", text(v.clone()));
        }
        if let Some(v) = f.mfa_enabled {
            set("mfa_enabled", flag(v));
        }
        if let Some(v) = &f.mfa_secret_enc {
            set("mfa_secret_enc", text(v.clone()));
        }
        if let Some(v) = &f.pending_mfa_secret_enc {
            set("mfa_pending_secret_enc", text(v.clone()));
        }
        if let Some(v) = f.mfa_last_step {
            set("mfa_last_step", SqlValue::Integer(v));
        }
        if let Some(v) = f.status {
            set("status", SqlValue::Text(v.as_str().into()));
        }
        if let Some(v) = f.accept_challenges {
            set("accept_challenges", flag(v));
        }
        if let Some(v) = f.last_login_at {
            set("last_login_at", int(v));
        }
        if let Some(v) = f.deleted_at {
            set("deleted_at", int(v));
        }
        if cols.is_empty() {
            return Ok(self.by_id(id)?.is_some());
        }
        let assignments: Vec<String> =
            cols.iter().enumerate().map(|(i, c)| format!("{c} = ?{}", i + 1)).collect();
        let sql = format!("UPDATE users SET {} WHERE id = ?{}", assignments.join(", "), cols.len() + 1);
        vals.push(SqlValue::Integer(id.into()));
        let n = self.db.exec(&sql, params_from_iter(vals)).map_err(StoreError::user_clash)?;
        Ok(n > 0)
    }

    /// Anonymizes an account (DESIGN 7): username `deleted#<id>` (also in its game records); the
    /// address, password, MFA, sessions, tokens, SSO links, recovery codes, integrity record and
    /// stored addresses of its security events are erased; ratings, games, conduct, sanctions,
    /// reports and refunds are kept. Error `not_found` for an unknown account.
    pub fn anonymize(&self, id: UserId, now: i64) -> Result<Anonymized> {
        self.db.transaction(|db| {
            if db.one("SELECT 1 FROM users WHERE id = ?1", [id], |_| Ok(()))?.is_none() {
                return Err(StoreError::not_found("no such user"));
            }
            let name = format!("deleted#{id}");
            let token_hashes =
                db.all("SELECT token_hash FROM sessions WHERE user_id = ?1 AND revoked_at IS NULL", [id], |r| r.get(0))?;
            db.exec("DELETE FROM sessions WHERE user_id = ?1", [id])?;
            db.exec("DELETE FROM tokens WHERE user_id = ?1", [id])?;
            db.exec("DELETE FROM sso_identities WHERE user_id = ?1", [id])?;
            db.exec("DELETE FROM mfa_recovery_codes WHERE user_id = ?1", [id])?;
            db.exec("DELETE FROM player_integrity WHERE user_id = ?1", [id])?;
            db.exec("UPDATE security_events SET ip = NULL WHERE user_id = ?1 AND ip IS NOT NULL", [id])?;
            db.exec("UPDATE games SET white_name = ?1 WHERE white_id = ?2", params![name, id])?;
            db.exec("UPDATE games SET black_name = ?1 WHERE black_id = ?2", params![name, id])?;
            db.exec(
                "UPDATE users SET username = ?1, username_lower = ?1, email = NULL, email_normalized = NULL,
                 email_verified = 0, password_hash = NULL, mfa_enabled = 0, mfa_secret_enc = NULL,
                 mfa_pending_secret_enc = NULL, status = 'deleted', accept_challenges = 0, deleted_at = ?2 WHERE id = ?3",
                params![name, now, id],
            )?;
            Ok(Anonymized { token_hashes })
        })
    }

    /// TOTP replay protection: `true` only when `step` is newer than the last accepted one (one
    /// conditional statement).
    pub fn advance_mfa_step(&self, id: UserId, step: i64) -> Result<bool> {
        let n = self.db.exec(
            "UPDATE users SET mfa_last_step = ?1 WHERE id = ?2 AND mfa_last_step < ?1",
            params![step, id],
        )?;
        Ok(n == 1)
    }
}

/// MFA recovery codes (their HMAC hashes).
#[derive(Debug, Clone, Copy)]
pub struct Mfa<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Mfa<'_> {
    /// Replaces every code of the account (duplicates collapse).
    pub fn replace_recovery_codes(&self, user_id: UserId, hashes: &[String], now: i64) -> Result<()> {
        self.db.transaction(|db| {
            db.exec("DELETE FROM mfa_recovery_codes WHERE user_id = ?1", [user_id])?;
            for h in hashes {
                db.exec(
                    "INSERT OR IGNORE INTO mfa_recovery_codes (user_id, code_hash, created_at) VALUES (?1, ?2, ?3)",
                    params![user_id, h, now],
                )?;
            }
            Ok(())
        })
    }

    /// Uses a code: `true` when it existed (deleted in one statement).
    pub fn consume_recovery_code(&self, user_id: UserId, hash: &str) -> Result<bool> {
        let n = self.db.exec(
            "DELETE FROM mfa_recovery_codes WHERE user_id = ?1 AND code_hash = ?2",
            params![user_id, hash],
        )?;
        Ok(n == 1)
    }

    /// Codes left.
    pub fn count_recovery_codes(&self, user_id: UserId) -> Result<i64> {
        self.db.count("SELECT count(*) FROM mfa_recovery_codes WHERE user_id = ?1", [user_id])
    }
}
