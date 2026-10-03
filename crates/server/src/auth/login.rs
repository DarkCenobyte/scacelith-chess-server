//! Password login, the MFA step, and the brute-force defences around them.
//!
//! * Per account: failures are counted per login string (lower case, so the counter exists for
//!   unknown logins too and the answers stay identical); from `AUTH_FAILURES_PER_ACCOUNT` failures
//!   on, each attempt must wait exponentially longer (2 s, 4 s, ... up to 15 min): 429
//!   `too_many_attempts` with `retryAfter`. The password step of a Google link is the same check
//!   under the same counter (the account's username), counted before its hash so that parallel
//!   requests are all counted.
//! * Whole server: every failed check feeds the login failure detector; above
//!   `POW_LOGIN_TRIGGER_PER_MIN` failures a minute, every login needs a proof of work
//!   (`POW_LOGIN_BITS`) for the next 5 minutes.
//! * Unknown account, wrong password and account without a usable password (Google-only, or a
//!   `!` hash) do the same hashing work in the same hash queue and give the same
//!   `invalid_credentials` answer after the same time (a failed check is padded to the slowest
//!   recent check). `email_unverified` and `banned` are only answered after the password matched.
//! * A matching outdated hash is upgraded only when a hash slot is free at once, and only while
//!   the stored hash is unchanged; a login whose password a reset replaced during the check fails.
//! * The MFA step: an `mfa_` token (5 min, 5 wrong codes at most, single use) and a per-account
//!   failure counter. The step of a password login holds a digest of the password hash it was
//!   issued for (`pwh`): a password reset or change between the two steps ends it, so no session
//!   opens with a password the reset replaced.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Map, Value, json};

use super::error::{AuthError, AuthResult};
use super::mfa::SecondFactor;
use super::tokens::{
    MFA_LOGIN, MFA_LOGIN_TTL_MS, MFA_PREFIX, data_of, is_live, is_prefixed_token, str_field,
};
use super::{Inner, LoginParams, MfaLoginParams, PowAnswer};
use crate::log_warn;
use crate::metrics::{self, CounterVec};
use crate::security::keys::{random_token, sha256_hex};
use crate::security::password::{BusyReason, HashBudget, HashError};
use crate::store::{NewToken, StoreError, User, UserStatus, UserUpdate};

/// Wrong codes an MFA step accepts before it ends.
pub const MFA_TOKEN_ATTEMPTS: i64 = 5;

static LOGINS: LazyLock<CounterVec> =
    LazyLock::new(|| metrics::counter_vec("scacelith_auth_logins_total", "Login attempts", &["result"]));
static POW: LazyLock<CounterVec> = LazyLock::new(|| {
    metrics::counter_vec("scacelith_auth_pow_total", "Proof-of-work answers", &["endpoint", "result"])
});

/// The digest of a stored password hash kept in an MFA step instead of the hash itself.
pub(crate) fn password_hash_digest(hash: Option<&str>) -> String {
    sha256_hex(hash.unwrap_or(""))
}

/// The stored hash a login checks: `None` for no password or a disabled (`!`) one.
fn usable_hash(user: Option<&User>) -> Option<&str> {
    user?.password_hash.as_deref().filter(|h| !h.starts_with('!'))
}

fn invalid_mfa_token() -> AuthError {
    AuthError::new(401, "invalid_mfa_token", "The login step expired; log in again.")
}

/// False when the step was issued for a password hash that is no longer the stored one. Steps
/// without `pwh` (a Google sign-in) are not bound.
fn same_password(data: &Map<String, Value>, user: &User) -> bool {
    match data.get("pwh") {
        Some(Value::String(pwh)) => *pwh == password_hash_digest(user.password_hash.as_deref()),
        _ => true,
    }
}

/// How a password check is made by [`Inner::verify_password`].
pub(crate) struct PasswordCheck<'a> {
    /// The failure counter's key.
    pub key: String,
    /// The account (`None`: unknown).
    pub user: Option<&'a User>,
    pub password: &'a str,
    pub ip: Option<&'a str>,
    pub pow: Option<&'a PowAnswer>,
    /// `password`, or `google_link` (noted in the failure events).
    pub method: &'static str,
    /// The hash of a Google link ticket whose try is reserved before the hash, and where to put
    /// the tries taken (this one included).
    pub reserve_link: Option<(&'a str, &'a AtomicI64)>,
}

impl Inner {
    /// True while the login proof of work is on.
    pub(crate) fn login_pow_active(&self) -> bool {
        self.login_pow.active()
    }

    /// Fails with 428 `pow_required` unless `given` is a valid, fresh answer for this endpoint and
    /// client.
    pub(crate) fn require_pow(
        &self,
        endpoint: &'static str,
        bits: u32,
        ip: Option<&str>,
        given: Option<&PowAnswer>,
    ) -> AuthResult<()> {
        let addr = ip.unwrap_or("");
        let again = |reason: &str| {
            POW.with(&[endpoint, reason]).inc();
            AuthError::new(428, "pow_required", "Proof of work required.")
                .with("reason", reason)
                .with("pow", self.pow.issue(addr, endpoint, bits).to_json())
        };
        let Some(given) = given else {
            self.events.record("pow_required", None, ip, Some(json!({ "endpoint": endpoint, "bits": bits })));
            return Err(again("required"));
        };
        if let Err(refusal) = self.pow.verify(addr, endpoint, bits, &given.challenge, &given.nonce) {
            let reason = refusal.as_str();
            self.events.record(
                "pow_failed",
                None,
                ip,
                Some(json!({ "endpoint": endpoint, "reason": reason })),
            );
            return Err(again(reason));
        }
        POW.with(&[endpoint, "ok"]).inc();
        Ok(())
    }

    /// Counts a failed password check for the login proof of work.
    fn note_login_failure(&self) {
        if let Some(source) = self.login_pow.note_failure() {
            self.events.record("login_pow_on", None, None, Some(json!({ "source": source.as_str() })));
        }
    }

    /// Refuses a banned account, or an unconfirmed one when confirmation is required.
    /// `address_proven`: the request proved the address itself (a Google link), so only the ban
    /// counts.
    pub(crate) async fn check_account_allowed(&self, user: &User, address_proven: bool) -> AuthResult<()> {
        if let Some(ban) = self.store.sanctions().active_ban(user.id, self.now()).await? {
            return Err(AuthError::new(403, "banned", "This account is banned.").with("until", ban.ends_at));
        }
        if !address_proven && self.config.require_email_verification && !user.email_verified {
            return Err(AuthError::new(
                403,
                "email_unverified",
                "Confirm your e-mail address first: open the link we sent you.",
            ));
        }
        Ok(())
    }

    /// Opens the session and builds the login answer `{token, expiresAt, user}`.
    pub(crate) async fn session_answer(
        &self,
        user: &User,
        client_label: Option<&str>,
        ip: Option<&str>,
        method: &str,
    ) -> AuthResult<Value> {
        let s = self.sessions.create(user, client_label, ip).await?;
        let update = UserUpdate { last_login_at: Some(Some(self.now())), ..UserUpdate::default() };
        // Informative only: the session is open whatever happens to this write.
        let _ = self.store.users().update(user.id, update).await;
        LOGINS.with(&["ok"]).inc();
        self.events.record("login", Some(user.id), ip, Some(json!({ "method": method })));
        let fresh = self.store.users().by_id(user.id).await?;
        let view = self.account_view(fresh.as_ref().unwrap_or(user)).await;
        Ok(json!({ "token": s.token, "expiresAt": s.expires_at, "user": view }))
    }

    /// After the first factor: the MFA challenge `{mfaRequired, mfaToken, expiresIn}` when two-step
    /// verification is on, else the session. `extra`: more data for the MFA step (the Google link
    /// it stores).
    pub(crate) async fn finish_login(
        &self,
        user: &User,
        client_label: Option<&str>,
        ip: Option<&str>,
        method: &str,
        extra: Option<Map<String, Value>>,
    ) -> AuthResult<Value> {
        if !user.mfa_enabled {
            return self.session_answer(user, client_label, ip, method).await;
        }
        let token = random_token(MFA_PREFIX);
        let mut data = Map::new();
        data.insert("attempts".into(), 0.into());
        data.insert("clientLabel".into(), client_label.into());
        data.insert("method".into(), method.into());
        data.extend(extra.unwrap_or_default());
        // A password login's step is only good for the password hash it was issued for.
        if method == "password" {
            data.insert("pwh".into(), password_hash_digest(user.password_hash.as_deref()).into());
        }
        let now = self.now();
        self.store
            .tokens()
            .create(NewToken {
                kind: MFA_LOGIN.into(),
                token_hash: sha256_hex(&token),
                user_id: Some(user.id),
                data: Some(Value::Object(data)),
                created_at: now,
                expires_at: now + MFA_LOGIN_TTL_MS,
            })
            .await?;
        LOGINS.with(&["mfa_required"]).inc();
        Ok(json!({ "mfaRequired": true, "mfaToken": token, "expiresIn": MFA_LOGIN_TTL_MS / 1000 }))
    }

    /// The active account named by a login (an address when it contains `@`, else a username).
    async fn find_login_user(&self, login: &str) -> AuthResult<Option<User>> {
        let user = self.store.users().by_login(login.to_owned()).await?;
        Ok(user.filter(|u| u.status == UserStatus::Active))
    }

    /// The password check of a login (see [`PasswordCheck`]): the wait of a counter at its
    /// threshold (429), the login proof of work while it is on (428), the reservation of a link
    /// try, then the check. Unknown account, wrong password and account without a usable password
    /// do the same work and fail alike (401 `invalid_credentials`, counted). Returns the account as
    /// stored now (after a rehash), the password matching it.
    pub(crate) async fn verify_password(&self, check: PasswordCheck<'_>) -> AuthResult<User> {
        let PasswordCheck { key, user, password, ip, pow, method, reserve_link } = check;
        let wait = self.failures.retry_after(&key);
        if wait > 0 {
            LOGINS.with(&["throttled"]).inc();
            self.events.record("login_throttled", None, ip, Some(json!({ "retryAfterMs": wait })));
            return Err(AuthError::too_many_attempts(wait));
        }
        if self.login_pow.active() {
            self.require_pow("login", self.login_pow.bits(), ip, pow)?;
        }
        let reserved = match reserve_link {
            Some((hash, tries)) => {
                tries.store(self.reserve_sso_link_try(hash).await?, Ordering::SeqCst);
                Some(self.failures.fail(&key))
            }
            None => None,
        };
        let budget = self.hasher.budget(ip);
        let stored = usable_hash(user);
        // Unknown account or no password: the dummy check, in the same queue, padded alike.
        let verified = self.hasher.check_password(stored, password, &budget.next()).await?;
        let mut current = None;
        if let (true, Some(u), Some(stored)) = (verified.ok, user, stored) {
            let checked = if verified.needs_rehash {
                self.rehash(u.id, stored, password, &budget).await
            } else {
                stored.to_owned()
            };
            current = self.still_current(u.id, &checked, password, &budget).await?;
        }
        let Some(current) = current else {
            let f = reserved.unwrap_or_else(|| self.failures.fail(&key));
            self.note_login_failure();
            LOGINS.with(&["failed"]).inc();
            let user_id = user.map(|u| u.id);
            let mut detail = json!({ "failures": f.failures });
            if method != "password" {
                detail["method"] = method.into();
            }
            self.events.record("login_failed", user_id, ip, Some(detail));
            if i64::from(f.failures) == self.config.auth_failures_per_account {
                let mut detail = json!({ "retryAfterMs": f.retry_after_ms });
                if method != "password" {
                    detail["method"] = method.into();
                }
                self.events.record("login_lockout", user_id, ip, Some(detail));
            }
            return Err(AuthError::invalid_credentials());
        };
        self.failures.reset(&key);
        Ok(current)
    }

    /// Upgrades an outdated hash with the password just checked, only when a hash slot is free at
    /// once and only while the stored hash is still the one checked. Returns the hash the account
    /// should now have.
    async fn rehash(&self, user_id: u32, stored: &str, password: &str, budget: &HashBudget) -> String {
        let message = match self.hasher.hash(password, &budget.no_wait()).await {
            Ok(next) => match self.set_password_hash_if(user_id, stored, &next).await {
                Ok(true) => return next,
                Ok(false) => return stored.to_owned(),
                Err(e) => e.message().into_owned(),
            },
            Err(HashError::Busy(BusyReason::NoWait)) => return stored.to_owned(),
            Err(e) => e.to_string(),
        };
        log_warn!(self.log, "password rehash failed", { "userId": user_id, "err": { "message": message } });
        stored.to_owned()
    }

    /// `POST /auth/login`.
    pub(crate) async fn login(&self, p: &LoginParams) -> AuthResult<Value> {
        let key = format!("l:{}", crate::security::encoding::js_trim(&p.login).to_lowercase());
        let user = self.find_login_user(&p.login).await?;
        let current = self
            .verify_password(PasswordCheck {
                key,
                user: user.as_ref(),
                password: &p.password,
                ip: p.ip.as_deref(),
                pow: p.pow.as_ref(),
                method: "password",
                reserve_link: None,
            })
            .await?;
        self.check_account_allowed(&current, false).await?;
        self.finish_login(&current, p.client_label.as_deref(), p.ip.as_deref(), "password", None).await
    }

    /// `POST /auth/login/mfa`.
    pub(crate) async fn login_mfa(&self, p: &MfaLoginParams) -> AuthResult<Value> {
        let ip = p.ip.as_deref();
        if !is_prefixed_token(&p.mfa_token, MFA_PREFIX) {
            return Err(invalid_mfa_token());
        }
        let h = sha256_hex(&p.mfa_token);
        let row = self.store.tokens().get(MFA_LOGIN.into(), h.clone()).await?;
        let Some(row) = row.filter(|r| is_live(Some(r), self.now())) else {
            return Err(invalid_mfa_token());
        };
        let data = data_of(&row);
        let user = match row.user_id {
            Some(id) => self.store.users().by_id(id).await?,
            None => None,
        };
        // Checked before the code, so that a recovery code is not spent on a dead step.
        let Some(user) =
            user.filter(|u| u.status == UserStatus::Active && u.mfa_enabled && same_password(&data, u))
        else {
            self.store.tokens().consume(MFA_LOGIN.into(), h, self.now()).await?;
            return Err(invalid_mfa_token());
        };
        let fkey = format!("m{}", user.id);
        let wait = self.mfa_failures.retry_after(&fkey);
        if wait > 0 {
            return Err(AuthError::too_many_attempts(wait));
        }
        let factor = SecondFactor { code: p.code.as_deref(), recovery_code: p.recovery_code.as_deref() };
        if factor.is_empty() {
            return Err(AuthError::new(400, "invalid_request", "A code or a recovery code is required."));
        }
        if !self.check_second_factor(&user, factor, true, ip).await? {
            self.mfa_failures.fail(&fkey);
            let (live, attempts) = self.count_mfa_failure(&h, &data).await?;
            self.events.record("mfa_failed", Some(user.id), ip, Some(json!({ "attempts": attempts })));
            if !live {
                return Err(invalid_mfa_token());
            }
            return Err(AuthError::new(401, "invalid_code", "Wrong or already used code."));
        }
        if self.store.tokens().consume(MFA_LOGIN.into(), h, self.now()).await?.is_none() {
            return Err(invalid_mfa_token());
        }
        self.mfa_failures.reset(&fkey);
        let fresh = self.store.users().by_id(user.id).await?.unwrap_or(user);
        // Again after the code check: a reset that landed meanwhile wins.
        if fresh.status != UserStatus::Active || !same_password(&data, &fresh) {
            return Err(invalid_mfa_token());
        }
        let link = data.get("link").filter(|l| l.is_object());
        self.check_account_allowed(&fresh, link.is_some()).await?;
        // The step of a Google link: the link is stored only now, after the account check (a
        // banned account gets none) and before the session.
        if let Some(link) = link {
            self.sso_link_proven(&fresh, link, ip, true).await?;
        }
        let method =
            format!("{}+totp", str_field(&data, "method").filter(|m| !m.is_empty()).unwrap_or("password"));
        self.session_answer(&fresh, str_field(&data, "clientLabel"), ip, &method).await
    }

    /// Counts a wrong code on the MFA step `h` as it is now (other codes may have been counted
    /// while this one was checked), in one transaction: the 5th ends it. Returns whether the step
    /// is still live and the attempts counted.
    async fn count_mfa_failure(&self, h: &str, issued: &Map<String, Value>) -> AuthResult<(bool, i64)> {
        let (now, h, issued) = (self.now(), h.to_owned(), issued.clone());
        Ok(self
            .store
            .write(move |db| {
                let current = db.tokens().get(MFA_LOGIN, &h)?;
                let live = is_live(current.as_ref(), now);
                let mut counted = current.as_ref().map_or(issued, data_of);
                let attempts = counted.get("attempts").and_then(Value::as_f64).map_or(0, |a| a as i64) + 1;
                if live && attempts >= MFA_TOKEN_ATTEMPTS {
                    db.tokens().consume(MFA_LOGIN, &h, now)?;
                } else if live {
                    counted.insert("attempts".into(), attempts.into());
                    db.tokens().update(MFA_LOGIN, &h, Some(&Value::Object(counted)))?;
                }
                Ok::<_, StoreError>((live, attempts))
            })
            .await?)
    }

    /// Stores `new_hash` for `user_id` only while the stored hash is still `expected` (compare
    /// and set): a hash computed from a password checked against `expected` never overwrites a
    /// password reset or change that landed meanwhile. True when written.
    pub(crate) async fn set_password_hash_if(
        &self,
        user_id: u32,
        expected: &str,
        new_hash: &str,
    ) -> AuthResult<bool> {
        let (expected, new_hash) = (expected.to_owned(), new_hash.to_owned());
        Ok(self
            .store
            .write(move |db| {
                let Some(u) = db.users().by_id(user_id)? else { return Ok(false) };
                if u.status != UserStatus::Active || u.password_hash.as_deref() != Some(expected.as_str()) {
                    return Ok(false);
                }
                db.users().update(
                    user_id,
                    &UserUpdate { password_hash: Some(Some(new_hash)), ..UserUpdate::default() },
                )?;
                Ok::<_, StoreError>(true)
            })
            .await?)
    }

    /// After `password` matched the hash `checked` of `user_id`: the account when the password
    /// still matches what is stored now, else `None`. A reset or change (or another login's
    /// rehash) may have landed during the check; only then is the password checked again, against
    /// the new hash.
    pub(crate) async fn still_current(
        &self,
        user_id: u32,
        checked: &str,
        password: &str,
        budget: &HashBudget,
    ) -> AuthResult<Option<User>> {
        let Some(u) = self.store.users().by_id(user_id).await? else { return Ok(None) };
        let Some(hash) =
            u.password_hash.as_deref().filter(|h| u.status == UserStatus::Active && !h.is_empty())
        else {
            return Ok(None);
        };
        if hash == checked {
            return Ok(Some(u));
        }
        if !self.hasher.verify(hash, password, &budget.next()).await?.ok {
            return Ok(None);
        }
        let again = self.store.users().by_id(user_id).await?;
        Ok(again.filter(|a| a.status == UserStatus::Active && a.password_hash == u.password_hash))
    }
}
