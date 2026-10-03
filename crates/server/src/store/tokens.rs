//! Single-use tokens, pending signups and SSO identities.

use rusqlite::{Row, params};
use serde_json::Value;

use super::db::Db;
use super::error::{ErrorKind, Result, StoreError};
use super::values::{check_username, clean_email, json_text, json_value, normalize_email, username_lower};
use crate::ids::UserId;

/// A new single-use token.
#[derive(Debug, Clone, Default)]
pub struct NewToken {
    /// What the token is for (`verify_email`, `reset_password`, `mfa_login`...).
    pub kind: String,
    /// SHA-256 of the token (lowercase hex).
    pub token_hash: String,
    pub user_id: Option<UserId>,
    /// Any JSON value (`None` or JSON null: SQL NULL).
    pub data: Option<Value>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// A stored token.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub id: i64,
    pub kind: String,
    pub user_id: Option<UserId>,
    pub data: Option<Value>,
    pub created_at: i64,
    pub expires_at: i64,
    pub consumed_at: Option<i64>,
}

const TOKEN_COLS: &str = "id, kind, user_id, data, created_at, expires_at, consumed_at";

fn to_token(r: &Row<'_>) -> rusqlite::Result<Token> {
    Ok(Token {
        id: r.get(0)?,
        kind: r.get(1)?,
        user_id: r.get(2)?,
        data: json_value(r.get(3)?),
        created_at: r.get(4)?,
        expires_at: r.get(5)?,
        consumed_at: r.get(6)?,
    })
}

/// The single-use tokens table.
#[derive(Debug, Clone, Copy)]
pub struct Tokens<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Tokens<'_> {
    /// Stores a token; returns its id. Error `duplicate` for a (kind, hash) already stored.
    pub fn create(&self, t: &NewToken) -> Result<i64> {
        self.db.insert(
            "INSERT INTO tokens (kind, token_hash, user_id, data, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![t.kind, t.token_hash, t.user_id, json_text(t.data.as_ref()), t.created_at, t.expires_at],
        )
    }

    /// Uses a token (one conditional statement, atomic across connections): the row the first
    /// time, `None` once consumed or expired.
    pub fn consume(&self, kind: &str, hash: &str, now: i64) -> Result<Option<Token>> {
        self.db.one(
            &format!(
                "UPDATE tokens SET consumed_at = ?3 WHERE kind = ?1 AND token_hash = ?2 AND consumed_at IS NULL
                 AND expires_at > ?3 RETURNING {TOKEN_COLS}"
            ),
            params![kind, hash, now],
            to_token,
        )
    }

    /// The token in any state.
    pub fn get(&self, kind: &str, hash: &str) -> Result<Option<Token>> {
        self.db.one(
            &format!("SELECT {TOKEN_COLS} FROM tokens WHERE kind = ?1 AND token_hash = ?2"),
            params![kind, hash],
            to_token,
        )
    }

    /// Counts one try on a live token whose `data.tries` is below `max` (one statement): the row
    /// with the try counted, `None` when the token is not live or has no try left.
    pub fn reserve_try(&self, kind: &str, hash: &str, max: i64, now: i64) -> Result<Option<Token>> {
        self.db.one(
            &format!(
                "UPDATE tokens SET data = json_set(coalesce(data, '{{}}'), '$.tries', coalesce(json_extract(data, '$.tries'), 0) + 1)
                 WHERE kind = ?1 AND token_hash = ?2 AND consumed_at IS NULL AND expires_at > ?4
                 AND coalesce(json_extract(data, '$.tries'), 0) < ?3 RETURNING {TOKEN_COLS}"
            ),
            params![kind, hash, max, now],
            to_token,
        )
    }

    /// Replaces the data of a token (any state); `true` when it exists.
    pub fn update(&self, kind: &str, hash: &str, data: Option<&Value>) -> Result<bool> {
        let n = self.db.exec(
            "UPDATE tokens SET data = ?1 WHERE kind = ?2 AND token_hash = ?3",
            params![json_text(data), kind, hash],
        )?;
        Ok(n > 0)
    }

    /// Deletes every token of `kind` of the account (consumed or not); returns how many.
    pub fn delete_for_user(&self, user_id: UserId, kind: &str) -> Result<usize> {
        self.db.exec("DELETE FROM tokens WHERE user_id = ?1 AND kind = ?2", params![user_id, kind])
    }

    /// The newest token of `kind` of the account that is neither consumed nor expired.
    pub fn live_for_user(&self, user_id: UserId, kind: &str, now: i64) -> Result<Option<Token>> {
        self.db.one(
            &format!(
                "SELECT {TOKEN_COLS} FROM tokens WHERE user_id = ?1 AND kind = ?2 AND consumed_at IS NULL
                 AND expires_at > ?3 ORDER BY created_at DESC, id DESC LIMIT 1"
            ),
            params![user_id, kind, now],
            to_token,
        )
    }
}

/// A new pending signup.
#[derive(Debug, Clone, Default)]
pub struct NewSignup {
    pub username: String,
    pub email: String,
    pub password_hash: String,
    /// SHA-256 of the confirmation link's token (`None`: no link was mailed).
    pub token_hash: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// A pending signup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signup {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub token_hash: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

const SIGNUP_COLS: &str = "id, username, email, password_hash, token_hash, created_at, expires_at";

fn to_signup(r: &Row<'_>) -> rusqlite::Result<Signup> {
    Ok(Signup {
        id: r.get(0)?,
        username: r.get(1)?,
        email: r.get(2)?,
        password_hash: r.get(3)?,
        token_hash: r.get(4)?,
        created_at: r.get(5)?,
        expires_at: r.get(6)?,
    })
}

/// Signups waiting for the confirmation of their address.
#[derive(Debug, Clone, Copy)]
pub struct Signups<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Signups<'_> {
    /// Stores a signup; returns its id. Errors `username_taken` / `email_taken` when another
    /// pending signup has the name or the address (accounts are not checked: the caller does it in
    /// its transaction), `invalid`.
    pub fn create(&self, s: &NewSignup) -> Result<i64> {
        check_username(&s.username)?;
        self.db
            .insert(
                "INSERT INTO pending_signups (username, username_lower, email, email_normalized, password_hash,
                 token_hash, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    s.username,
                    username_lower(&s.username),
                    clean_email(&s.email),
                    normalize_email(&s.email),
                    s.password_hash,
                    s.token_hash,
                    s.created_at,
                    s.expires_at,
                ],
            )
            .map_err(StoreError::user_clash)
    }

    /// The signup holding this username (case-insensitive), expired included.
    pub fn by_username(&self, name: &str) -> Result<Option<Signup>> {
        self.db.one(
            &format!("SELECT {SIGNUP_COLS} FROM pending_signups WHERE username_lower = ?1"),
            [username_lower(name)],
            to_signup,
        )
    }

    /// The signup of this address (normalized first), expired included.
    pub fn by_email(&self, email: &str) -> Result<Option<Signup>> {
        let Some(n) = normalize_email(email) else { return Ok(None) };
        self.db.one(
            &format!("SELECT {SIGNUP_COLS} FROM pending_signups WHERE email_normalized = ?1"),
            [n],
            to_signup,
        )
    }

    /// The signup of a link's token hash, expired included.
    pub fn by_token_hash(&self, hash: &str) -> Result<Option<Signup>> {
        self.db.one(
            &format!("SELECT {SIGNUP_COLS} FROM pending_signups WHERE token_hash = ?1"),
            [hash],
            to_signup,
        )
    }

    /// A new link (`None`: none) and expiry; `true` when the signup exists.
    pub fn renew(&self, id: i64, token_hash: Option<&str>, expires_at: i64) -> Result<bool> {
        let n = self.db.exec(
            "UPDATE pending_signups SET token_hash = ?1, expires_at = ?2 WHERE id = ?3",
            params![token_hash, expires_at, id],
        )?;
        Ok(n > 0)
    }

    /// Deletes a signup; `true` when it existed.
    pub fn delete(&self, id: i64) -> Result<bool> {
        Ok(self.db.exec("DELETE FROM pending_signups WHERE id = ?1", [id])? > 0)
    }
}

/// An SSO identity of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsoIdentity {
    pub provider: String,
    pub subject: String,
    pub email: Option<String>,
    pub created_at: i64,
}

/// The account an SSO identity is linked to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsoLink {
    pub user_id: UserId,
    pub email: Option<String>,
}

/// SSO identities.
#[derive(Debug, Clone, Copy)]
pub struct Sso<'a> {
    pub(crate) db: &'a Db<'a>,
}

impl Sso<'_> {
    /// The account of an identity.
    pub fn find(&self, provider: &str, subject: &str) -> Result<Option<SsoLink>> {
        self.db.one(
            "SELECT user_id, email FROM sso_identities WHERE provider = ?1 AND subject = ?2",
            params![provider, subject],
            |r| Ok(SsoLink { user_id: r.get(0)?, email: r.get(1)? }),
        )
    }

    /// Links an identity to an account (a re-link of the same account updates the address and
    /// keeps the creation time). Error `sso_taken` when another account has it.
    pub fn link(
        &self,
        user_id: UserId,
        provider: &str,
        subject: &str,
        email: Option<&str>,
        now: i64,
    ) -> Result<()> {
        let n = self.db.exec(
            "INSERT INTO sso_identities (provider, subject, user_id, email, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (provider, subject) DO UPDATE SET email = excluded.email WHERE user_id = excluded.user_id",
            params![provider, subject, user_id, email.and_then(clean_email), now],
        )?;
        if n == 0 {
            return Err(StoreError::new(ErrorKind::SsoTaken, "identity linked to another account"));
        }
        Ok(())
    }

    /// The identities of an account, oldest first.
    pub fn for_user(&self, user_id: UserId) -> Result<Vec<SsoIdentity>> {
        self.db.all(
            "SELECT provider, subject, email, created_at FROM sso_identities WHERE user_id = ?1 ORDER BY created_at",
            [user_id],
            |r| Ok(SsoIdentity { provider: r.get(0)?, subject: r.get(1)?, email: r.get(2)?, created_at: r.get(3)? }),
        )
    }
}
