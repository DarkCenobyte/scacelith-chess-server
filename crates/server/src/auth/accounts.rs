//! Account flows: registration, e-mail confirmation, password reset and change, the account view,
//! re-authenticated account changes (MFA, e-mail address, deletion), preferences, sessions and
//! logout.
//!
//! Enumeration resistance: register, resend and forgot give the same answer whatever the address;
//! the "address taken" branch of a registration hashes the password too (same work) and mails the
//! owner of the address instead. E-mails are throttled per address (single-use keys, the same call
//! made whether the address exists or not).
//!
//! Pending signups (with `REQUIRE_EMAIL_VERIFICATION`): a registration creates no account. The
//! signup (username, address, password hash, the SHA-256 of its link's token) waits for the life
//! of the link (24 h) and holds its username in both branches: a new address gets the link (at
//! most one link mail per address every 5 minutes), an address that already has an account gets
//! no link and its owner a notice. A username held by a live pending signup of another address is
//! refused as a taken one; a new signup with the same address replaces the pending one. Using the
//! link creates the account, its address confirmed, and deletes the signup in one transaction.
//!
//! E-mail change: 400 `invalid_email` / `same_email` before the password is checked. With
//! confirmation required: 202 whether or not another account uses the address; an `email_change`
//! token (24 h, data `{email, from}`) replaces the user's pending change; the new address gets the
//! link (at most one every 5 minutes), or its owner, when it belongs to another account, a notice
//! (at most one per hour); the current address is told. The confirmation applies the change in
//! one transaction with the end of the former address's links; the unique index decides a race.
//! Without confirmation: the change happens at once (200) or 409 `email_taken`.
//!
//! A password change or reset cancels a pending e-mail change and the other reset links; account
//! changes are written only while the password the request proved is still the stored one
//! (compare and set): a reset that landed meanwhile wins.

use serde_json::{Map, Value, json};

use super::error::{AuthError, AuthResult};
use super::identity::{check_username, is_valid_email, mask_email, normalize_email, normalize_opt_email};
use super::mfa::SecondFactor;
use super::tokens::{
    EMAIL_CHANGE, EMAIL_CHANGE_TTL_MS, EMAIL_VERIFY, EMAIL_VERIFY_TTL_MS, PASSWORD_RESET,
    PASSWORD_RESET_TTL_MS, data_of, is_link_token, is_live, str_field,
};
use super::{Answer, Credentials, EmailChangeOutcome, Inner, LinkOutcome, RegisterParams, SessionInfo};
use crate::config::Registration;
use crate::ids::UserId;
use crate::mail::Template;
use crate::security::encoding::b64_url;
use crate::security::keys::{hmac_sha256, random_token, sha256_hex};
use crate::security::password::{HashBudget, PolicyViolation, check_password_policy};
use crate::store::{
    Db, ErrorKind, NewSignup, NewToken, NewUser, Signup, StoreError, Token, User, UserStatus, UserUpdate,
};

/// One link mail per address and kind within this time.
pub const MAIL_THROTTLE_MS: i64 = 5 * 60_000;
/// One notice to the owner of an address within this time.
pub const NOTICE_THROTTLE_MS: i64 = 60 * 60_000;

/// Which second factor a re-authentication asks for when two-step verification is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FactorRule {
    /// The password only.
    None,
    /// The password and an authenticator code.
    Totp,
    /// The password and an authenticator code or a recovery code.
    Any,
}

/// A refused password answered 400 `weak_password` with its reason.
fn weak(policy: PolicyViolation) -> AuthError {
    AuthError::new(400, "weak_password", policy.message).with("reason", policy.reason.as_str())
}

fn username_taken() -> AuthError {
    AuthError::new(409, "username_taken", "This username is already taken.")
}

fn email_taken() -> AuthError {
    AuthError::new(409, "email_taken", "An account already uses this e-mail address.")
}

fn invalid_email() -> AuthError {
    AuthError::new(400, "invalid_email", "This e-mail address is not valid.")
}

fn invalid_password() -> AuthError {
    AuthError::new(403, "invalid_password", "Wrong password.")
}

fn invalid_reset_token() -> AuthError {
    AuthError::new(400, "invalid_token", "This reset link is invalid, was already used, or has expired.")
}

fn mfa_not_enabled() -> AuthError {
    AuthError::new(409, "mfa_not_enabled", "Two-step verification is not enabled.")
}

/// A store that stayed locked past its busy timeout is answered 503 `server_busy` (retry in 1 s):
/// nothing changed, the same request or link can be sent again.
pub(crate) fn store_busy(e: StoreError) -> AuthError {
    if e.kind() == ErrorKind::Busy { AuthError::server_busy(Some(1)) } else { e.into() }
}

fn is_active(u: &User) -> bool {
    u.status == UserStatus::Active
}

/// Whether a live pending signup of another address holds `username`.
pub(crate) fn username_held(db: &Db<'_>, username: &str, email: &str, now: i64) -> Result<bool, StoreError> {
    Ok(db
        .signups()
        .by_username(username)?
        .is_some_and(|p| p.expires_at > now && normalize_email(&p.email) != normalize_email(email)))
}

/// Whether the account of a link token (data `{email}`) is active and still has the address the
/// link was mailed to.
fn sent_to_current_address(db: &Db<'_>, row: &Token) -> Result<bool, StoreError> {
    let Some(user) = (match row.user_id {
        Some(id) => db.users().by_id(id)?,
        None => None,
    }) else {
        return Ok(false);
    };
    let data = data_of(row);
    Ok(is_active(&user)
        && normalize_opt_email(user.email.as_deref()) == normalize_opt_email(str_field(&data, "email")))
}

/// The live pending signup of a link token.
fn live_signup(db: &Db<'_>, token: &str, now: i64) -> Result<Option<Signup>, StoreError> {
    if !is_link_token(token) {
        return Ok(None);
    }
    Ok(db.signups().by_token_hash(&sha256_hex(token))?.filter(|p| p.expires_at > now))
}

/// Ends the links mailed to an account's former address.
fn drop_address_links(db: &Db<'_>, user_id: UserId) -> Result<(), StoreError> {
    for kind in [EMAIL_CHANGE, PASSWORD_RESET, EMAIL_VERIFY] {
        db.tokens().delete_for_user(user_id, kind)?;
    }
    Ok(())
}

impl Inner {
    /// The single-use key of the mail throttle of `kind` for `email` (no address in the key).
    pub(crate) fn mail_key(&self, kind: &str, email: &str) -> String {
        let mac = b64_url(&hmac_sha256(self.keys.mail_throttle.as_bytes(), email.as_bytes()));
        format!("mail:{kind}:{}", &mac[..24])
    }

    /// True the first time `key` is used within `ttl_ms`.
    pub(crate) fn once(&self, key: &str, ttl_ms: i64) -> bool {
        self.control.consume_once(key, ttl_ms)
    }

    /// Queues an e-mail (never waited for: the mailer logs its failures).
    pub(crate) fn mail(&self, template: Template<'_>, to: Option<&str>) {
        if let Some(to) = to.filter(|t| !t.is_empty()) {
            drop(self.mailer.send_template(&template, to));
        }
    }

    fn link(&self, page: &str, token: &str) -> String {
        format!("{}/{page}?token={token}", self.base_url)
    }

    /// The account of a session, or 401 `invalid_token` when it is gone or no longer active.
    pub(crate) async fn active_user(&self, id: UserId) -> AuthResult<User> {
        match self.store.users().by_id(id).await? {
            Some(u) if is_active(&u) => Ok(u),
            _ => Err(AuthError::new(401, "invalid_token", "The session is invalid; log in again.")),
        }
    }

    /// The account as its player sees it (the `user` of `GET /account/me` and of the login
    /// answers): `{id, username, email, emailVerified, mfaEnabled, googleLinked, hasPassword,
    /// acceptChallenges ('all' | 'none'), createdAt, lastLoginAt, pendingEmail}`.
    pub(crate) async fn account_view(&self, user: &User) -> Value {
        let (id, now) = (user.id, self.now());
        let extra = self
            .store
            .read(move |db| {
                let google = db.sso().for_user(id)?.iter().any(|l| l.provider == "google");
                let pending = db.tokens().live_for_user(id, EMAIL_CHANGE, now)?.and_then(|row| {
                    str_field(&data_of(&row), "email").filter(|e| !e.is_empty()).map(str::to_owned)
                });
                Ok::<_, StoreError>((google, pending))
            })
            .await;
        // Informative only: a failed read shows no link and no pending change.
        let (google, pending) = extra.unwrap_or((false, None));
        json!({
            "id": user.id,
            "username": user.username,
            "email": user.email,
            "emailVerified": user.email_verified,
            "mfaEnabled": user.mfa_enabled,
            "googleLinked": google,
            "hasPassword": user.password_hash.as_deref().is_some_and(|h| !h.is_empty()),
            "acceptChallenges": if user.accept_challenges { "all" } else { "none" },
            "createdAt": user.created_at,
            "lastLoginAt": user.last_login_at,
            "pendingEmail": pending,
        })
    }

    fn password_policy(&self, password: &str, username: &str, email: &str) -> AuthResult<()> {
        let min = usize::try_from(self.config.password_min_length.max(0)).unwrap_or(usize::MAX);
        check_password_policy(password, min, username, email).map_err(weak)
    }

    async fn notify_existing_address(&self, user: &User, ip: Option<&str>) {
        self.events.record("register_existing_email", Some(user.id), ip, None);
        let email = user.email.as_deref().unwrap_or("");
        if self.once(&self.mail_key("regattempt", email), NOTICE_THROTTLE_MS) {
            self.mail(
                Template::RegistrationAttempt { username: &user.username, email_change: false },
                Some(email),
            );
        }
    }

    /// The owner of an address another player asked to move their account to. `fresh`: the hourly
    /// throttle of this notice. No address in the owner's event: it would be the other player's.
    fn notify_address_claimed(&self, owner: &User, fresh: bool) {
        self.events.record("email_change_existing_email", Some(owner.id), None, None);
        if fresh {
            self.mail(
                Template::RegistrationAttempt { username: &owner.username, email_change: true },
                owner.email.as_deref(),
            );
        }
    }

    fn mail_signup_link(&self, username: &str, email: &str, token: &str) {
        let link = self.link("verify-email", token);
        let hours = EMAIL_VERIFY_TTL_MS / 3_600_000;
        self.mail(Template::Verification { username, link: &link, hours }, Some(email));
    }

    /// `POST /auth/register`: 202 `{status: 'verification_sent'}` (confirmation required) or 201
    /// `{status: 'ready'}`.
    pub(crate) async fn register(&self, p: &RegisterParams) -> AuthResult<Answer> {
        let ip = p.ip.as_deref();
        if self.config.registration != Registration::Open {
            return Err(AuthError::new(403, "registration_closed", "Registration is closed on this server."));
        }
        check_username(&p.username, self.usernames)
            .map_err(|m| AuthError::new(400, "invalid_username", m))?;
        let em = normalize_email(&p.email);
        if !is_valid_email(&em) {
            return Err(invalid_email());
        }
        self.password_policy(&p.password, &p.username, &em)?;
        let (username, now) = (p.username.clone(), self.now());
        let email = em.clone();
        let taken = self
            .store
            .read(move |db| {
                Ok::<_, StoreError>(
                    db.users().by_username(&username)?.is_some()
                        || username_held(db, &username, &email, now)?,
                )
            })
            .await?;
        if taken {
            return Err(username_taken());
        }
        if self.config.pow_register_bits > 0 {
            let bits = u32::try_from(self.config.pow_register_bits).unwrap_or(u32::MAX);
            self.require_pow("register", bits, ip, p.pow.as_ref())?;
        }
        let existing = self.store.users().by_email(em.clone()).await?;
        // Also for an address that has an account: the same work.
        let password_hash = self.hasher.hash(&p.password, &self.hasher.budget(ip).next()).await?;
        if self.config.require_email_verification {
            let token = existing.is_none().then(|| random_token(""));
            if !self.hold_signup(&p.username, &em, password_hash, token.as_deref()).await? {
                return Err(username_taken());
            }
            match (&existing, token) {
                (Some(owner), _) => self.notify_existing_address(owner, ip).await,
                (None, Some(token)) => {
                    if self.once(&self.mail_key("signup", &em), MAIL_THROTTLE_MS) {
                        self.mail_signup_link(&p.username, &em, &token);
                    }
                }
                (None, None) => {}
            }
            return Ok(Answer { status: 202, body: json!({ "status": "verification_sent" }) });
        }
        if let Some(owner) = &existing {
            self.notify_existing_address(owner, ip).await;
            return Err(email_taken());
        }
        let new_user = NewUser {
            username: p.username.clone(),
            email: Some(em.clone()),
            password_hash: Some(password_hash),
            email_verified: true,
            accept_challenges: true,
            created_at: self.now(),
        };
        let id = match self.store.users().create(new_user).await {
            Ok(id) => id,
            Err(e) if e.kind() == ErrorKind::UsernameTaken => return Err(username_taken()),
            Err(e) if e.kind() == ErrorKind::EmailTaken => {
                if let Some(owner) = self.store.users().by_email(em).await? {
                    self.notify_existing_address(&owner, ip).await;
                }
                return Err(email_taken());
            }
            Err(e) => return Err(e.into()),
        };
        self.events.record("register", Some(id), ip, None);
        Ok(Answer { status: 201, body: json!({ "status": "ready" }) })
    }

    /// Stores the pending signup of `email` (replacing the address's previous one), in one
    /// transaction with the checks of its username. False: an account or a live pending signup
    /// of another address took the username since the first check.
    async fn hold_signup(
        &self,
        username: &str,
        email: &str,
        password_hash: String,
        token: Option<&str>,
    ) -> AuthResult<bool> {
        let t = self.now();
        let signup = NewSignup {
            username: username.to_owned(),
            email: email.to_owned(),
            password_hash,
            token_hash: token.map(sha256_hex),
            created_at: t,
            expires_at: t + EMAIL_VERIFY_TTL_MS,
        };
        self.store
            .write(move |db| {
                if db.users().by_username(&signup.username)?.is_some()
                    || username_held(db, &signup.username, &signup.email, t)?
                {
                    return Ok(false);
                }
                let previous =
                    [db.signups().by_username(&signup.username)?, db.signups().by_email(&signup.email)?];
                for p in previous.into_iter().flatten() {
                    db.signups().delete(p.id)?;
                }
                db.signups().create(&signup)?;
                Ok(true)
            })
            .await
            .map_err(store_busy)
    }

    /// A live token row of `kind` for a link token (not consumed). A password reset link also
    /// needs the account to have the address it was mailed to.
    pub(crate) async fn peek_token(&self, kind: &'static str, token: &str) -> AuthResult<Option<Token>> {
        if !is_link_token(token) {
            return Ok(None);
        }
        let (hash, now) = (sha256_hex(token), self.now());
        Ok(self
            .store
            .read(move |db| {
                let Some(row) = db.tokens().get(kind, &hash)?.filter(|r| is_live(Some(r), now)) else {
                    return Ok(None);
                };
                let ok = kind != PASSWORD_RESET || sent_to_current_address(db, &row)?;
                Ok::<_, StoreError>(ok.then_some(row))
            })
            .await?)
    }

    /// Whether a confirmation link (an account's, or a pending signup's) is live
    /// (`GET /verify-email`).
    pub(crate) async fn peek_verification(&self, token: &str) -> AuthResult<bool> {
        if !is_link_token(token) {
            return Ok(false);
        }
        if self.peek_token(EMAIL_VERIFY, token).await?.is_some() {
            return Ok(true);
        }
        let (token, now) = (token.to_owned(), self.now());
        Ok(self.store.read(move |db| live_signup(db, &token, now)).await?.is_some())
    }

    /// `POST /verify-email`: uses the link and confirms the address, in one transaction (a busy
    /// store: 503, the link still works). The link of a pending signup creates its account.
    pub(crate) async fn verify_email(&self, token: &str, ip: Option<&str>) -> AuthResult<LinkOutcome> {
        if !is_link_token(token) {
            return Ok(LinkOutcome::Invalid);
        }
        let (hash, now) = (sha256_hex(token), self.now());
        let confirmed = self
            .store
            .write(move |db| {
                let Some(row) = db.tokens().consume(EMAIL_VERIFY, &hash, now)? else { return Ok(None) };
                let Some(u) = (match row.user_id {
                    Some(id) => db.users().by_id(id)?,
                    None => None,
                }) else {
                    return Ok(None);
                };
                let data = data_of(&row);
                if !is_active(&u)
                    || normalize_opt_email(u.email.as_deref())
                        != normalize_opt_email(str_field(&data, "email"))
                {
                    return Ok(None);
                }
                if !u.email_verified {
                    db.users()
                        .update(u.id, &UserUpdate { email_verified: Some(true), ..UserUpdate::default() })?;
                }
                Ok::<_, StoreError>(Some(u.id))
            })
            .await
            .map_err(store_busy)?;
        let Some(user_id) = confirmed else { return self.confirm_signup(token, ip).await };
        self.sessions.refresh(user_id);
        self.events.record("email_verified", Some(user_id), ip, None);
        Ok(LinkOutcome::Confirmed)
    }

    /// The link of a pending signup: its account is created, the address confirmed, and the
    /// signup deleted, in one transaction. `Taken`: another account took the username or the
    /// address since the signup (the signup is dropped).
    async fn confirm_signup(&self, token: &str, ip: Option<&str>) -> AuthResult<LinkOutcome> {
        let (token, now) = (token.to_owned(), self.now());
        let created = self
            .store
            .write(move |db| {
                let Some(p) = live_signup(db, &token, now)? else { return Ok(Err(LinkOutcome::Invalid)) };
                db.signups().delete(p.id)?;
                if db.users().by_username(&p.username)?.is_some() || db.users().by_email(&p.email)?.is_some()
                {
                    return Ok(Err(LinkOutcome::Taken));
                }
                let user = NewUser {
                    username: p.username,
                    email: Some(p.email),
                    password_hash: Some(p.password_hash),
                    email_verified: true,
                    accept_challenges: true,
                    created_at: now,
                };
                match db.users().create(&user) {
                    Ok(id) => Ok(Ok(id)),
                    Err(e) if matches!(e.kind(), ErrorKind::UsernameTaken | ErrorKind::EmailTaken) => {
                        Ok(Err(LinkOutcome::Taken))
                    }
                    Err(e) => Err(e),
                }
            })
            .await
            .map_err(store_busy)?;
        match created {
            Ok(id) => {
                self.events.record("register", Some(id), ip, None);
                Ok(LinkOutcome::Confirmed)
            }
            Err(outcome) => Ok(outcome),
        }
    }

    /// `POST /auth/verify-email/resend`: `{status: 'accepted'}` whatever happens.
    pub(crate) async fn resend_verification(&self, email: &str, ip: Option<&str>) -> AuthResult<Value> {
        let accepted = json!({ "status": "accepted" });
        let em = normalize_email(email);
        let fresh = self.once(&self.mail_key("verify", &em), MAIL_THROTTLE_MS);
        if !fresh || !is_valid_email(&em) {
            return Ok(accepted);
        }
        let user = self.store.users().by_email(em.clone()).await?;
        if let Some(u) = user.as_ref().filter(|u| is_active(u) && !u.email_verified) {
            self.send_verification(u).await?;
            self.events.record("verification_resent", Some(u.id), ip, None);
        }
        // The address's pending signup gets its 24 h again whether or not the address has an
        // account (how long its username stays held must not tell), in the same transaction in
        // both cases; a new link replaces the previous one only when the address has no account.
        // Best effort: a busy store renews nothing and the answer is still the same.
        let token = random_token("");
        let (now, has_user, new_hash) = (self.now(), user.is_some(), sha256_hex(&token));
        let renewed = self
            .store
            .write(move |db| {
                let Some(row) = db.signups().by_email(&em)?.filter(|r| r.expires_at > now) else {
                    return Ok(None);
                };
                let hash = if has_user { row.token_hash.clone() } else { Some(new_hash) };
                db.signups().renew(row.id, hash.as_deref(), now + EMAIL_VERIFY_TTL_MS)?;
                Ok::<_, StoreError>(Some(row))
            })
            .await;
        let renewed = match renewed {
            Ok(r) => r,
            Err(e) if e.kind() == ErrorKind::Busy => None,
            Err(e) => return Err(e.into()),
        };
        if let (Some(p), false) = (renewed, has_user) {
            self.mail_signup_link(&p.username, &p.email, &token);
        }
        Ok(accepted)
    }

    async fn send_verification(&self, user: &User) -> AuthResult<()> {
        let token = random_token("");
        let now = self.now();
        self.store
            .tokens()
            .create(NewToken {
                kind: EMAIL_VERIFY.into(),
                token_hash: sha256_hex(&token),
                user_id: Some(user.id),
                data: Some(json!({ "email": user.email })),
                created_at: now,
                expires_at: now + EMAIL_VERIFY_TTL_MS,
            })
            .await?;
        let link = self.link("verify-email", &token);
        let hours = EMAIL_VERIFY_TTL_MS / 3_600_000;
        self.mail(
            Template::Verification { username: &user.username, link: &link, hours },
            user.email.as_deref(),
        );
        Ok(())
    }

    /// `POST /auth/password/forgot`: `{status: 'accepted'}` whatever happens.
    pub(crate) async fn forgot_password(&self, email: &str, ip: Option<&str>) -> AuthResult<Value> {
        let em = normalize_email(email);
        let fresh = self.once(&self.mail_key("reset", &em), MAIL_THROTTLE_MS);
        let user = if is_valid_email(&em) { self.store.users().by_email(em).await? } else { None };
        if let Some(u) = user.filter(|u| fresh && is_active(u)) {
            let token = random_token("");
            let now = self.now();
            self.store
                .tokens()
                .create(NewToken {
                    kind: PASSWORD_RESET.into(),
                    token_hash: sha256_hex(&token),
                    user_id: Some(u.id),
                    data: Some(json!({ "email": u.email })),
                    created_at: now,
                    expires_at: now + PASSWORD_RESET_TTL_MS,
                })
                .await?;
            let link = self.link("reset-password", &token);
            let minutes = PASSWORD_RESET_TTL_MS / 60_000;
            self.mail(
                Template::PasswordReset { username: &u.username, link: &link, minutes },
                u.email.as_deref(),
            );
            self.events.record("password_reset_requested", Some(u.id), ip, None);
        }
        Ok(json!({ "status": "accepted" }))
    }

    /// `POST /auth/password/reset` (and the HTML form): every session is revoked, two-step
    /// verification is untouched, the address counts as confirmed (the link proved it).
    pub(crate) async fn reset_password(
        &self,
        token: &str,
        new_password: &str,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        let Some(row) = self.peek_token(PASSWORD_RESET, token).await? else {
            return Err(invalid_reset_token());
        };
        let user = match row.user_id {
            Some(id) => self.store.users().by_id(id).await?,
            None => None,
        };
        let Some(user) = user.filter(is_active) else { return Err(invalid_reset_token()) };
        self.password_policy(new_password, &user.username, user.email.as_deref().unwrap_or(""))?;
        let password_hash = self.hasher.hash(new_password, &self.hasher.budget(ip).next()).await?;
        let (hash, now, id) = (sha256_hex(token), self.now(), user.id);
        // One transaction: no change of address lands between the check of the link's address and
        // the new password, and the pending e-mail change and the other reset links go with it.
        let done = self
            .store
            .write(move |db| {
                let Some(used) = db.tokens().consume(PASSWORD_RESET, &hash, now)? else { return Ok(false) };
                if !sent_to_current_address(db, &used)? {
                    return Ok(false);
                }
                db.users().update(
                    id,
                    &UserUpdate {
                        password_hash: Some(Some(password_hash)),
                        email_verified: Some(true),
                        ..UserUpdate::default()
                    },
                )?;
                db.tokens().delete_for_user(id, EMAIL_CHANGE)?;
                db.tokens().delete_for_user(id, PASSWORD_RESET)?;
                Ok::<_, StoreError>(true)
            })
            .await
            .map_err(store_busy)?;
        if !done {
            return Err(invalid_reset_token());
        }
        self.sessions.revoke_all(user.id, None).await?;
        self.events.record("password_reset", Some(user.id), ip, None);
        self.mail(
            Template::PasswordChanged { username: &user.username, when_ms: self.now(), by_reset: true },
            user.email.as_deref(),
        );
        Ok(json!({ "status": "password_reset" }))
    }

    /// Re-authentication for account changes: the password and, when two-step verification is on
    /// and `rule` is not [`FactorRule::None`], an authenticator code ([`FactorRule::Totp`]) or a
    /// code or recovery code ([`FactorRule::Any`]). Returns the account; its password hash is the
    /// hash the password matched.
    pub(crate) async fn reauth(
        &self,
        user_id: UserId,
        password: &str,
        factor: SecondFactor<'_>,
        rule: FactorRule,
        ip: Option<&str>,
        budget: &HashBudget,
    ) -> AuthResult<User> {
        let key = format!("r{user_id}");
        let wait = self.reauth_failures.retry_after(&key);
        if wait > 0 {
            return Err(AuthError::too_many_attempts(wait));
        }
        let user = self.active_user(user_id).await?;
        let Some(hash) = user.password_hash.as_deref().filter(|h| !h.is_empty()) else {
            self.hasher.verify_dummy(password, &budget.next()).await?;
            return Err(AuthError::new(
                400,
                "password_not_set",
                "This account has no password yet; set one with \"Forgot password\" first.",
            ));
        };
        let ok = self.hasher.verify(hash, password, &budget.next()).await?.ok;
        // The password may have been reset or changed while the check waited and ran.
        let current = if ok { self.still_current(user_id, hash, password, budget).await? } else { None };
        let Some(current) = current else {
            self.reauth_failures.fail(&key);
            self.events.record("reauth_failed", Some(user_id), ip, Some(json!({ "factor": "password" })));
            return Err(invalid_password());
        };
        if rule != FactorRule::None && current.mfa_enabled {
            if factor.is_empty() {
                return Err(AuthError::new(
                    403,
                    "mfa_code_required",
                    "Enter a code of your authenticator app.",
                ));
            }
            if !self.check_second_factor(&current, factor, rule == FactorRule::Any, ip).await? {
                self.reauth_failures.fail(&key);
                self.events.record("reauth_failed", Some(user_id), ip, Some(json!({ "factor": "mfa" })));
                return Err(AuthError::new(403, "invalid_code", "Wrong or already used code."));
            }
            self.reauth_failures.reset(&key);
            // The row after the second factor (its MFA state), unless the password changed meanwhile.
            let after = self.store.users().by_id(user_id).await?;
            return Ok(after.filter(|a| a.password_hash == current.password_hash).unwrap_or(current));
        }
        self.reauth_failures.reset(&key);
        Ok(current)
    }

    /// `POST /account/password`: the other sessions are revoked.
    pub(crate) async fn change_password(
        &self,
        session: &SessionInfo,
        current_password: &str,
        new_password: &str,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        // Both hashes of the request share one queue timeout.
        let budget = self.hasher.budget(ip);
        let user = self
            .reauth(session.user_id, current_password, SecondFactor::default(), FactorRule::None, ip, &budget)
            .await?;
        self.password_policy(new_password, &user.username, user.email.as_deref().unwrap_or(""))?;
        let next = self.hasher.hash(new_password, &budget.next()).await?;
        let checked = user.password_hash.as_deref().unwrap_or("");
        // Written only over the hash the current password matched: a reset or another change
        // that landed meanwhile wins (a login's rehash of the same password is no change).
        if !self.set_password_hash_if(user.id, checked, &next).await? {
            let fresh = self.still_current(user.id, checked, current_password, &budget).await?;
            let written = match fresh.as_ref().and_then(|f| f.password_hash.as_deref()) {
                Some(h) => self.set_password_hash_if(user.id, h, &next).await?,
                None => false,
            };
            if !written {
                return Err(invalid_password());
            }
        }
        self.sessions.revoke_all(user.id, Some(session.session_id)).await?;
        let id = user.id;
        self.store
            .write(move |db| {
                db.tokens().delete_for_user(id, EMAIL_CHANGE)?;
                db.tokens().delete_for_user(id, PASSWORD_RESET)?;
                Ok::<_, StoreError>(())
            })
            .await?;
        self.events.record("password_changed", Some(user.id), ip, None);
        self.mail(
            Template::PasswordChanged { username: &user.username, when_ms: self.now(), by_reset: false },
            user.email.as_deref(),
        );
        Ok(json!({ "status": "password_changed" }))
    }

    /// `GET /account/me`: `{user, ratings, sanctions, ban}`.
    pub(crate) async fn me(&self, user_id: UserId) -> AuthResult<Value> {
        let user = self.active_user(user_id).await?;
        let t = self.now();
        let (ratings, sanctions, ban) = self
            .store
            .read(move |db| {
                Ok::<_, StoreError>((
                    db.ratings().for_user(user_id)?,
                    db.sanctions().list(user_id)?,
                    db.sanctions().active_ban(user_id, t)?,
                ))
            })
            .await?;
        let ratings: Vec<Value> = ratings
            .iter()
            .map(|r| {
                json!({
                    "category": r.category,
                    "rating": r.record.rating,
                    "games": r.record.games,
                    "wins": r.record.wins,
                    "draws": r.record.draws,
                    "losses": r.record.losses,
                    "peak": r.record.peak,
                    "provisional": r.provisional,
                })
            })
            .collect();
        let sanctions: Vec<Value> = sanctions
            .iter()
            .filter(|s| s.lifted_at.is_none() && s.starts_at <= t && s.ends_at.is_none_or(|e| e > t))
            .map(|s| {
                json!({ "kind": s.kind.as_str(), "reason": s.reason, "startsAt": s.starts_at, "endsAt": s.ends_at })
            })
            .collect();
        Ok(json!({
            "user": self.account_view(&user).await,
            "ratings": ratings,
            "sanctions": sanctions,
            "ban": ban.map(|b| json!({ "until": b.ends_at })),
        }))
    }

    /// `POST /account/mfa/totp/setup`.
    pub(crate) async fn mfa_setup(
        &self,
        session: &SessionInfo,
        password: &str,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        let pre = self.active_user(session.user_id).await?;
        if pre.mfa_enabled {
            return Err(AuthError::new(
                409,
                "mfa_already_enabled",
                "Two-step verification is already enabled.",
            ));
        }
        let budget = self.hasher.budget(ip);
        let user = self
            .reauth(session.user_id, password, SecondFactor::default(), FactorRule::None, ip, &budget)
            .await?;
        self.events.record("mfa_setup_started", Some(user.id), ip, None);
        self.mfa_setup_secret(&user).await
    }

    /// `POST /account/mfa/totp/enable`.
    pub(crate) async fn mfa_enable(
        &self,
        session: &SessionInfo,
        code: &str,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        let key = format!("r{}", session.user_id);
        let wait = self.reauth_failures.retry_after(&key);
        if wait > 0 {
            return Err(AuthError::too_many_attempts(wait));
        }
        let user = self.active_user(session.user_id).await?;
        let Some(codes) = self.mfa_activate(&user, code).await? else {
            self.reauth_failures.fail(&key);
            return Err(AuthError::new(
                403,
                "invalid_code",
                "Wrong code; check the time of your device and try again.",
            ));
        };
        self.reauth_failures.reset(&key);
        self.events.record("mfa_enabled", Some(user.id), ip, None);
        Ok(json!({ "status": "mfa_enabled", "recoveryCodes": codes }))
    }

    /// `POST /account/mfa/totp/disable`.
    pub(crate) async fn mfa_disable(
        &self,
        session: &SessionInfo,
        creds: &Credentials,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        let pre = self.active_user(session.user_id).await?;
        if !pre.mfa_enabled {
            return Err(mfa_not_enabled());
        }
        let factor = creds.factor();
        if factor.is_empty() {
            return Err(AuthError::new(
                403,
                "mfa_code_required",
                "Enter a code of your authenticator app or a recovery code.",
            ));
        }
        let budget = self.hasher.budget(ip);
        let user =
            self.reauth(session.user_id, &creds.password, factor, FactorRule::Any, ip, &budget).await?;
        self.mfa_turn_off(&user).await?;
        self.events.record("mfa_disabled", Some(user.id), ip, None);
        self.mail(
            Template::MfaDisabled { username: &user.username, when_ms: self.now() },
            user.email.as_deref(),
        );
        Ok(json!({ "status": "mfa_disabled" }))
    }

    /// `POST /account/mfa/recovery-codes`.
    pub(crate) async fn regenerate_recovery_codes(
        &self,
        session: &SessionInfo,
        password: &str,
        code: Option<&str>,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        let pre = self.active_user(session.user_id).await?;
        if !pre.mfa_enabled {
            return Err(mfa_not_enabled());
        }
        let budget = self.hasher.budget(ip);
        let factor = SecondFactor { code, recovery_code: None };
        let user = self.reauth(session.user_id, password, factor, FactorRule::Totp, ip, &budget).await?;
        let codes = self.mfa_new_recovery_codes(&user).await?;
        self.events.record("recovery_codes_regenerated", Some(user.id), ip, None);
        Ok(json!({ "recoveryCodes": codes }))
    }

    /// `POST /account/delete`: anonymises the account and revokes every session. The security
    /// events still waiting in the batch (this request's `recovery_code_used`, the last second's
    /// logins) are written first, so that the anonymisation erases their addresses too; the
    /// deletion's own event has none.
    pub(crate) async fn delete_account(
        &self,
        session: &SessionInfo,
        creds: &Credentials,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        let budget = self.hasher.budget(ip);
        let user = self
            .reauth(session.user_id, &creds.password, creds.factor(), FactorRule::Any, ip, &budget)
            .await?;
        // The flush's write is queued before the anonymisation (the store runs writes in order).
        let flushed = self.events.flush();
        let anonymized = self.store.users().anonymize(user.id, self.now());
        flushed.await;
        anonymized.await?;
        self.sessions.revoked_elsewhere(user.id);
        self.events.record("account_deleted", Some(user.id), None, None);
        Ok(json!({ "status": "deleted" }))
    }

    /// `PUT /account/preferences` (`'all'` or `'none'`, stored as a boolean).
    pub(crate) async fn set_preferences(
        &self,
        session: &SessionInfo,
        accept_challenges: &str,
    ) -> AuthResult<Value> {
        let user = self.active_user(session.user_id).await?;
        let update =
            UserUpdate { accept_challenges: Some(accept_challenges != "none"), ..UserUpdate::default() };
        self.store.users().update(user.id, update).await?;
        Ok(json!({ "preferences": { "acceptChallenges": accept_challenges } }))
    }

    /// Runs `write` in one transaction only while the stored password is still the one the
    /// request proved (compare and set): a reset or change that landed since the check wins and
    /// the request is refused as with a wrong password; a sign-in's rehash of the same password
    /// is checked once against the new hash and is no change.
    async fn with_password<F>(
        &self,
        user: &User,
        password: &str,
        budget: &HashBudget,
        write: F,
    ) -> AuthResult<()>
    where
        F: Fn(&Db<'_>) -> Result<(), StoreError> + Clone + Send + 'static,
    {
        if self.write_if_password(user.id, user.password_hash.clone(), write.clone()).await? {
            return Ok(());
        }
        let checked = user.password_hash.as_deref().unwrap_or("");
        let Some(again) = self.still_current(user.id, checked, password, budget).await? else {
            return Err(invalid_password());
        };
        if self.write_if_password(user.id, again.password_hash, write).await? {
            Ok(())
        } else {
            Err(invalid_password())
        }
    }

    async fn write_if_password<F>(&self, user_id: UserId, hash: Option<String>, write: F) -> AuthResult<bool>
    where
        F: Fn(&Db<'_>) -> Result<(), StoreError> + Send + 'static,
    {
        self.store
            .write(move |db| {
                let Some(u) = db.users().by_id(user_id)? else { return Ok(false) };
                if !is_active(&u) || u.password_hash != hash {
                    return Ok(false);
                }
                write(db)?;
                Ok(true)
            })
            .await
            .map_err(store_busy)
    }

    /// Once the address of `user` changed from `from` to `email`: the cached sessions are read
    /// again, the change is recorded, the former address is told.
    fn email_changed(&self, user: &User, from: Option<&str>, email: &str, ip: Option<&str>) {
        self.sessions.refresh(user.id);
        self.events.record("email_changed", Some(user.id), ip, None);
        let masked = mask_email(email);
        self.mail(
            Template::EmailChanged { username: &user.username, masked_email: &masked, when_ms: self.now() },
            from,
        );
    }

    /// `POST /account/email` (module documentation).
    pub(crate) async fn change_email(
        &self,
        session: &SessionInfo,
        new_email: &str,
        creds: &Credentials,
        ip: Option<&str>,
    ) -> AuthResult<Answer> {
        let pre = self.active_user(session.user_id).await?;
        let em = normalize_email(new_email);
        if !is_valid_email(&em) {
            return Err(invalid_email());
        }
        let same =
            || AuthError::new(400, "same_email", "This is already the e-mail address of your account.");
        if normalize_opt_email(pre.email.as_deref()) == em {
            return Err(same());
        }
        let budget = self.hasher.budget(ip);
        let user = self
            .reauth(session.user_id, &creds.password, creds.factor(), FactorRule::Any, ip, &budget)
            .await?;
        if normalize_opt_email(user.email.as_deref()) == em {
            return Err(same());
        }
        let from = user.email.clone().filter(|e| !e.is_empty());
        let owner = self.store.users().by_email(em.clone()).await?.filter(|h| h.id != user.id);
        let id = user.id;

        if !self.config.require_email_verification {
            if let Some(o) = &owner {
                let fresh = self.once(&self.mail_key("emailchange", &em), NOTICE_THROTTLE_MS);
                self.notify_address_claimed(o, fresh);
                return Err(email_taken());
            }
            let email = em.clone();
            let written = self
                .with_password(&user, &creds.password, &budget, move |db| {
                    db.users().update(
                        id,
                        &UserUpdate {
                            email: Some(Some(email.clone())),
                            email_verified: Some(true),
                            ..UserUpdate::default()
                        },
                    )?;
                    drop_address_links(db, id)
                })
                .await;
            if let Err(e) = written {
                if e.store_error().is_some_and(|s| s.kind() == ErrorKind::EmailTaken) {
                    if let Some(o) = self.store.users().by_email(em.clone()).await?.filter(|o| o.id != id) {
                        let fresh = self.once(&self.mail_key("emailchange", &em), NOTICE_THROTTLE_MS);
                        self.notify_address_claimed(&o, fresh);
                    }
                    return Err(email_taken());
                }
                return Err(e);
            }
            self.email_changed(&user, from.as_deref(), &em, ip);
            return Ok(Answer { status: 200, body: json!({ "status": "email_changed", "email": em }) });
        }

        // The same work and the same answer whether or not the address is free.
        let fresh = self.once(&self.mail_key("emailchange", &em), NOTICE_THROTTLE_MS);
        // One confirmation link per new address every 5 minutes: within that time the request
        // keeps its pending change to that address, whose link was mailed already.
        let link_fresh = self.once(&self.mail_key("emailchange-link", &em), MAIL_THROTTLE_MS);
        let token = random_token("");
        let (token_hash, now) = (sha256_hex(&token), self.now());
        let data = json!({ "email": em, "from": from });
        let (email, from_norm) = (em.clone(), normalize_opt_email(from.as_deref()));
        self.with_password(&user, &creds.password, &budget, move |db| {
            if !link_fresh {
                let live = db
                    .tokens()
                    .live_for_user(id, EMAIL_CHANGE, now)?
                    .map(|r| data_of(&r))
                    .unwrap_or_default();
                if normalize_opt_email(str_field(&live, "email")) == email
                    && normalize_opt_email(str_field(&live, "from")) == from_norm
                {
                    return Ok(());
                }
            }
            db.tokens().delete_for_user(id, EMAIL_CHANGE)?;
            db.tokens().create(&NewToken {
                kind: EMAIL_CHANGE.into(),
                token_hash: token_hash.clone(),
                user_id: Some(id),
                data: Some(data.clone()),
                created_at: now,
                expires_at: now + EMAIL_CHANGE_TTL_MS,
            })?;
            Ok(())
        })
        .await?;
        self.events.record("email_change_requested", Some(id), ip, None);
        let hours = EMAIL_CHANGE_TTL_MS / 3_600_000;
        if let Some(o) = &owner {
            self.notify_address_claimed(o, fresh);
        } else if link_fresh {
            let link = self.link("confirm-email-change", &token);
            self.mail(
                Template::EmailChangeConfirm { username: &user.username, link: &link, hours },
                Some(&em),
            );
        }
        let masked = mask_email(&em);
        self.mail(
            Template::EmailChangeRequested {
                username: &user.username,
                masked_email: &masked,
                when_ms: self.now(),
                hours,
            },
            from.as_deref(),
        );
        Ok(Answer { status: 202, body: json!({ "status": "verification_sent" }) })
    }

    /// A live e-mail change link's account name and new address, without using it (the page
    /// `GET /confirm-email-change`).
    pub(crate) async fn peek_email_change(&self, token: &str) -> AuthResult<Option<(String, String)>> {
        let Some(row) = self.peek_token(EMAIL_CHANGE, token).await? else { return Ok(None) };
        let data = data_of(&row);
        let user = match row.user_id {
            Some(id) => self.store.users().by_id(id).await?,
            None => None,
        };
        Ok(user
            .filter(|u| {
                is_active(u)
                    && normalize_opt_email(u.email.as_deref())
                        == normalize_opt_email(str_field(&data, "from"))
            })
            .map(|u| (u.username, str_field(&data, "email").unwrap_or("").to_owned())))
    }

    /// `POST /confirm-email-change`: uses the link and applies the change (module documentation).
    pub(crate) async fn confirm_email_change(
        &self,
        token: &str,
        ip: Option<&str>,
    ) -> AuthResult<EmailChangeOutcome> {
        enum Done {
            Invalid,
            Taken(User),
            Changed(User, String),
        }
        if !is_link_token(token) {
            return Ok(EmailChangeOutcome::Invalid);
        }
        let (hash, now) = (sha256_hex(token), self.now());
        // One transaction: the link is used up, the address changes and the links of the former
        // address are dropped together, or nothing happens.
        let done = self
            .store
            .write(move |db| {
                let Some(row) = db.tokens().consume(EMAIL_CHANGE, &hash, now)? else {
                    return Ok(Done::Invalid);
                };
                let data = data_of(&row);
                let em = normalize_opt_email(str_field(&data, "email"));
                let user = match row.user_id {
                    Some(id) => db.users().by_id(id)?,
                    None => None,
                };
                let Some(user) = user.filter(|u| is_active(u) && is_valid_email(&em)) else {
                    return Ok(Done::Invalid);
                };
                // The account's address changed since the request: stale.
                if normalize_opt_email(user.email.as_deref()) != normalize_opt_email(str_field(&data, "from"))
                {
                    return Ok(Done::Invalid);
                }
                if db.users().by_email(&em)?.is_some_and(|h| h.id != user.id) {
                    return Ok(Done::Taken(user));
                }
                let update = UserUpdate {
                    email: Some(Some(em.clone())),
                    email_verified: Some(true),
                    ..UserUpdate::default()
                };
                match db.users().update(user.id, &update) {
                    Ok(_) => {}
                    // The unique index refused it: the link stays used up (the transaction commits).
                    Err(e) if e.kind() == ErrorKind::EmailTaken => return Ok(Done::Taken(user)),
                    Err(e) => return Err(e),
                }
                drop_address_links(db, user.id)?;
                Ok(Done::Changed(user, em))
            })
            .await
            .map_err(store_busy)?;
        match done {
            Done::Invalid => Ok(EmailChangeOutcome::Invalid),
            Done::Taken(user) => {
                self.events.record(
                    "email_change_refused",
                    Some(user.id),
                    ip,
                    Some(json!({ "reason": "email_taken" })),
                );
                Ok(EmailChangeOutcome::Taken)
            }
            Done::Changed(user, email) => {
                self.email_changed(&user, user.email.as_deref(), &email, ip);
                Ok(EmailChangeOutcome::Changed { email })
            }
        }
    }

    /// `POST /auth/logout`.
    pub(crate) async fn logout(&self, session: &SessionInfo, ip: Option<&str>) -> AuthResult<Value> {
        let hash = hex::encode(session.token_hash);
        self.sessions.revoke(session.user_id, session.session_id, Some(&hash)).await?;
        self.events.record("session_revoked", Some(session.user_id), ip, Some(json!({ "reason": "logout" })));
        Ok(json!({ "status": "logged_out" }))
    }

    /// `POST /auth/logout-all`.
    pub(crate) async fn logout_all(&self, session: &SessionInfo, ip: Option<&str>) -> AuthResult<Value> {
        self.sessions.revoke_all(session.user_id, None).await?;
        self.events.record(
            "sessions_revoked_all",
            Some(session.user_id),
            ip,
            Some(json!({ "reason": "logout_all" })),
        );
        Ok(json!({ "status": "logged_out" }))
    }

    /// `DELETE /auth/sessions/:id` (`id` as written in the path).
    pub(crate) async fn revoke_session(
        &self,
        session: &SessionInfo,
        id: &str,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        let Some(row) = self.sessions.find(session.user_id, id).await? else {
            return Err(AuthError::new(404, "not_found", "No such session."));
        };
        self.sessions.revoke(session.user_id, row.id, None).await?;
        self.events.record("session_revoked", Some(session.user_id), ip, Some(json!({ "reason": "user" })));
        Ok(json!({ "status": "revoked" }))
    }

    /// `GET /auth/sessions`: `{sessions: [...]}`.
    pub(crate) async fn list_sessions(&self, session: &SessionInfo) -> AuthResult<Value> {
        let list = self.sessions.list(session.user_id, Some(session.session_id)).await?;
        let mut out = Map::new();
        out.insert("sessions".into(), Value::Array(list));
        Ok(Value::Object(out))
    }
}
