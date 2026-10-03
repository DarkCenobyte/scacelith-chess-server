//! Google sign-in for a desktop game (DESIGN.md 5.9, API.md "Google sign-in"): the installed-app
//! flow with a loopback redirect (RFC 8252 section 7.3).
//!
//! 1. start: the game listens on 127.0.0.1, makes a PKCE pair and sends `{codeChallenge,
//!    redirectPort}`. The server creates an attempt (10 min) with its own state, nonce and PKCE
//!    pair for Google, and the redirect URI `http://127.0.0.1:<port>/oauth2/google/<this server's
//!    origin tag>`, built from the port alone. It answers the attempt id, the Google URL and the
//!    state.
//! 2. Google sends the browser to the game's listener with the code and the state.
//! 3. finish: the game posts the attempt id, its PKCE verifier (an attempt id alone is useless),
//!    the state and the code. The attempt is consumed; the state and the issuer are checked; the
//!    code is exchanged and the ID token verified. The answer: a login answer for a linked Google
//!    account, `{needsUsername, ssoTicket}` for a new account, or `{needsPassword, linkTicket,
//!    username}` when an account with a password uses the address.
//! 4. link: `{linkTicket, password}`: the account's password (5 tries per ticket, the login's
//!    failure counter and proof of work). The link is stored then, or, with two-step verification
//!    on, once the MFA step accepts a code; the account's address gets a notice.
//! 5. complete (new accounts): `{ssoTicket, username}` creates the account, links it and logs in.
//!
//! Invariant: a Google identity is attached to an existing account only after the person proved
//! the Google address (ID token, `email_verified`) and the account (its current password, plus its
//! second factor when on). It never depends on the account's own address confirmation.

use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Map, Value, json};

use super::accounts::username_held;
use super::error::{AuthError, AuthResult};
use super::identity::{check_username, normalize_email, normalize_opt_email, suggest_username};
use super::login::{PasswordCheck, ProvenPassword, password_hash_digest};
use super::oidc::{OidcClient, pkce_challenge};
use super::tokens::{
    SSO_ATTEMPT, SSO_LINK, SSO_PREFIX, SSO_TICKET, SSO_TTL_MS, data_of, is_live, is_prefixed_token, str_field,
};
use super::{Inner, SsoCompleteParams, SsoFinishParams, SsoLinkParams};
use crate::config::Registration;
use crate::ids::UserId;
use crate::log_warn;
use crate::mail::Template;
use crate::security::keys::{random_token, safe_eq, sha256_hex};
use crate::store::{ErrorKind, NewToken, NewUser, StoreError, User, UserStatus, UserUpdate};

const PROVIDER: &str = "google";
/// Password tries per link ticket.
pub const LINK_TRIES: i64 = 5;

/// The answer of a refused Google sign-in step.
fn fail(code: &'static str) -> AuthError {
    let (status, message) = match code {
        "sso_email_unverified" => {
            (403, "Google has not confirmed the e-mail address of this Google account.")
        }
        "sso_account_exists" => (
            409,
            "An account already uses this e-mail address and cannot be linked to Google sign-in here. Sign in to it as usual, or ask the server's operator.",
        ),
        "sso_already_linked" => (409, "This Google account was linked to another account meanwhile."),
        "sso_expired" => (410, "This sign-in has expired; start again from Scacelith."),
        "registration_closed" => (403, "Registration is closed on this server."),
        "account_disabled" => (403, "This account can no longer be used."),
        _ => (502, "Google sign-in could not be completed."),
    };
    AuthError::new(status, code, message)
}

fn expired() -> AuthError {
    fail("sso_expired")
}

/// A password the account can sign in with (not a Google-only account, not a `!` hash).
fn usable_password(user: &User) -> bool {
    user.password_hash.as_deref().is_some_and(|h| !h.starts_with('!'))
}

/// A JavaScript `String(value)` of a claim (`""` for none).
fn claim_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// The first claim of `keys` that JavaScript holds true (a non-empty string...), as text.
fn first_truthy(claims: &Map<String, Value>, keys: &[&str]) -> String {
    for k in keys {
        match claims.get(*k) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => {}
            Some(Value::String(s)) if s.is_empty() => {}
            Some(Value::Number(n)) if n.as_f64() == Some(0.0) => {}
            v => return claim_text(v),
        }
    }
    String::new()
}

/// What a verified Google identity signs in to.
enum Resolution {
    Login(UserId),
    New { sub: String, email: String, suggestion: String },
    Link { user_id: UserId, username: String, sub: String, email: String },
    Refused(&'static str),
}

/// The Google link a password (and second factor) proved: `{sub, email, pwh}`.
struct ProvenLink {
    sub: String,
    email: String,
    pwh: String,
}

impl ProvenLink {
    fn from_value(v: &Value) -> ProvenLink {
        let text = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
        ProvenLink { sub: text("sub"), email: text("email"), pwh: text("pwh") }
    }

    fn to_value(&self) -> Value {
        json!({ "sub": self.sub, "email": self.email, "pwh": self.pwh })
    }
}

impl Inner {
    /// True when Google sign-in is on.
    pub(crate) fn sso_enabled(&self) -> bool {
        self.config.sso_google_enabled && self.oidc.is_some()
    }

    fn require_sso(&self) -> AuthResult<&OidcClient> {
        match &self.oidc {
            Some(oidc) if self.config.sso_google_enabled => Ok(oidc),
            _ => Err(AuthError::new(404, "sso_disabled", "Google sign-in is not enabled on this server.")),
        }
    }

    /// `POST /auth/sso/google/start`: `{attemptId, authUrl, state, expiresIn}`.
    pub(crate) async fn sso_start(
        &self,
        code_challenge: &str,
        redirect_port: u16,
        ip: Option<&str>,
    ) -> AuthResult<Value> {
        let oidc = self.require_sso()?;
        let attempt_id = random_token(SSO_PREFIX);
        let (state, nonce, verifier) = (random_token(""), random_token(""), random_token(""));
        // From the port alone and this server's own tag: never a host, path or URI of the client.
        let redirect_uri =
            format!("http://127.0.0.1:{redirect_port}/oauth2/google/{}", self.config.sso_redirect_tag);
        let auth_url = oidc
            .authorization_url(&state, &nonce, &pkce_challenge(&verifier), &redirect_uri)
            .map_err(|e| AuthError::internal(e.to_string()))?;
        let now = self.now();
        self.store
            .tokens()
            .create(NewToken {
                kind: SSO_ATTEMPT.into(),
                token_hash: sha256_hex(&attempt_id),
                user_id: None,
                data: Some(json!({
                    "challenge": code_challenge,
                    "stateHash": sha256_hex(&state),
                    "nonce": nonce,
                    "verifier": verifier,
                    "redirectUri": redirect_uri,
                })),
                created_at: now,
                expires_at: now + SSO_TTL_MS,
            })
            .await?;
        self.events.record("sso_started", None, ip, Some(json!({ "provider": PROVIDER })));
        Ok(
            json!({ "attemptId": attempt_id, "authUrl": auth_url, "state": state, "expiresIn": SSO_TTL_MS / 1000 }),
        )
    }

    async fn resolve_account(&self, claims: &Map<String, Value>) -> AuthResult<Resolution> {
        let sub = claim_text(claims.get("sub"));
        if let Some(link) = self.store.sso().find(PROVIDER.into(), sub.clone()).await? {
            let user = self.store.users().by_id(link.user_id).await?;
            return Ok(match user {
                Some(u) if u.status == UserStatus::Active => Resolution::Login(u.id),
                _ => Resolution::Refused("account_disabled"),
            });
        }
        let email = normalize_email(&claim_text(claims.get("email")));
        let verified = matches!(claims.get("email_verified"), Some(Value::Bool(true)))
            || claims.get("email_verified").and_then(Value::as_str) == Some("true");
        if email.is_empty() || !verified {
            return Ok(Resolution::Refused("sso_email_unverified"));
        }
        if let Some(local) =
            self.store.users().by_email(email.clone()).await?.filter(|u| u.status == UserStatus::Active)
        {
            // Never linked here, whatever the account's address confirmation: the link step asks
            // for its password.
            return Ok(if usable_password(&local) {
                Resolution::Link { user_id: local.id, username: local.username, sub, email }
            } else {
                Resolution::Refused("sso_account_exists")
            });
        }
        if self.config.registration != Registration::Open {
            return Ok(Resolution::Refused("registration_closed"));
        }
        let mut suggestion = suggest_username(&first_truthy(claims, &["given_name", "name"]), self.usernames);
        if suggestion.is_empty() {
            let local_part = email.split('@').next().unwrap_or("");
            suggestion = suggest_username(local_part, self.usernames);
        }
        Ok(Resolution::New { sub, email, suggestion })
    }

    /// `POST /auth/sso/google/finish`: the code Google sent to the game's listener.
    pub(crate) async fn sso_finish(&self, p: &SsoFinishParams) -> AuthResult<Value> {
        let oidc = self.require_sso()?;
        let ip = p.ip.as_deref();
        if !is_prefixed_token(&p.attempt_id, SSO_PREFIX) {
            return Err(expired());
        }
        let h = sha256_hex(&p.attempt_id);
        let row = self.store.tokens().get(SSO_ATTEMPT.into(), h.clone()).await?;
        let Some(row) = row.filter(|r| is_live(Some(r), self.now())) else { return Err(expired()) };
        let d = data_of(&row);
        let (Some(state_hash), Some(redirect_uri), Some(verifier)) =
            (str_field(&d, "stateHash"), str_field(&d, "redirectUri"), str_field(&d, "verifier"))
        else {
            return Err(expired());
        };
        let challenge =
            claim_text(d.get("challenge").filter(|c| !matches!(c, Value::String(s) if s.is_empty())));
        if !safe_eq(pkce_challenge(&p.code_verifier).as_bytes(), challenge.as_bytes()) {
            self.events.record("sso_bad_verifier", None, ip, None);
            return Err(AuthError::new(
                403,
                "invalid_verifier",
                "This sign-in attempt belongs to another client.",
            ));
        }
        if self.store.tokens().consume(SSO_ATTEMPT.into(), h, self.now()).await?.is_none() {
            return Err(expired());
        }
        // Never Google's text, the code or the state in the answer, the log or the event.
        let failed = |reason: &str, message: Option<&str>| {
            match message {
                Some(m) => {
                    log_warn!(self.log, "google sign-in failed", { "reason": reason, "err": { "message": m } })
                }
                None => log_warn!(self.log, "google sign-in failed", { "reason": reason }),
            }
            self.events.record(
                "sso_failed",
                None,
                ip,
                Some(json!({ "provider": PROVIDER, "reason": reason })),
            );
            fail("sso_failed")
        };
        if !safe_eq(sha256_hex(&p.state).as_bytes(), state_hash.as_bytes()) {
            return Err(failed("state_mismatch", None));
        }
        if let Some(iss) = &p.iss
            && !oidc.issuers().iter().any(|i| i == iss)
        {
            return Err(failed("bad_iss", None));
        }
        let nonce = str_field(&d, "nonce").unwrap_or("");
        let claims = match oidc.exchange_code(&p.code, verifier, redirect_uri).await {
            Ok(id_token) => oidc.verify_id_token(&id_token, nonce).await,
            Err(e) => Err(e),
        };
        let claims = claims.map_err(|e| failed(e.reason, Some(&e.message)))?;
        let client_label = p.client_label.as_deref();
        match self.resolve_account(&claims).await? {
            Resolution::Login(id) => {
                let user = self.store.users().by_id(id).await?.filter(|u| u.status == UserStatus::Active);
                let Some(user) = user else { return Err(fail("account_disabled")) };
                self.check_account_allowed(&user, false).await?;
                self.events.record("sso_login", Some(user.id), ip, Some(json!({ "provider": PROVIDER })));
                self.finish_login(&user, client_label, ip, PROVIDER, None).await
            }
            Resolution::New { sub, email, suggestion } => {
                let ticket = random_token(SSO_PREFIX);
                let now = self.now();
                self.store
                    .tokens()
                    .create(NewToken {
                        kind: SSO_TICKET.into(),
                        token_hash: sha256_hex(&ticket),
                        user_id: None,
                        data: Some(json!({ "sub": sub, "email": email })),
                        created_at: now,
                        expires_at: now + SSO_TTL_MS,
                    })
                    .await?;
                Ok(json!({ "needsUsername": true, "ssoTicket": ticket, "suggestedUsername": suggestion }))
            }
            Resolution::Link { user_id, username, sub, email } => {
                let ticket = random_token(SSO_PREFIX);
                let now = self.now();
                self.store
                    .tokens()
                    .create(NewToken {
                        kind: SSO_LINK.into(),
                        token_hash: sha256_hex(&ticket),
                        user_id: Some(user_id),
                        data: Some(json!({ "userId": user_id, "sub": sub, "email": email, "tries": 0 })),
                        created_at: now,
                        expires_at: now + SSO_TTL_MS,
                    })
                    .await?;
                self.events.record(
                    "sso_link_required",
                    Some(user_id),
                    None,
                    Some(json!({ "provider": PROVIDER })),
                );
                Ok(
                    json!({ "needsPassword": true, "linkTicket": ticket, "username": username, "expiresIn": SSO_TTL_MS / 1000 }),
                )
            }
            Resolution::Refused(code) => Err(fail(code)),
        }
    }

    /// Reserves one password try of the link ticket `hash` (410 when it has none left or ended);
    /// returns the tries taken, this one included.
    pub(crate) async fn reserve_sso_link_try(&self, hash: &str) -> AuthResult<i64> {
        let row =
            self.store.tokens().reserve_try(SSO_LINK.into(), hash.to_owned(), LINK_TRIES, self.now()).await?;
        let Some(row) = row else { return Err(expired()) };
        Ok(data_of(&row).get("tries").and_then(Value::as_i64).unwrap_or(0))
    }

    /// Stores the Google link of `user` after its password (and, with `expect_mfa`, its second
    /// factor) was proven for `proven` (`{sub, email, pwh}`), in one transaction with the checks
    /// that the account is still the one proven: active, the same address and password hash,
    /// two-step verification as it was (410 `sso_expired` otherwise). 409 `sso_already_linked`
    /// when the identity was linked to another account meanwhile. The link confirms the address,
    /// and the address gets a notice.
    pub(crate) async fn sso_link_proven(
        &self,
        user: &User,
        proven: &Value,
        ip: Option<&str>,
        expect_mfa: bool,
    ) -> AuthResult<()> {
        let proven = ProvenLink::from_value(proven);
        let (id, now) = (user.id, self.now());
        let (sub, email, pwh) = (proven.sub.clone(), proven.email.clone(), proven.pwh.clone());
        let linked = self
            .store
            .write(move |db| {
                let u = db.users().by_id(id)?.filter(|u| u.status == UserStatus::Active);
                let Some(u) = u.filter(|u| {
                    normalize_opt_email(u.email.as_deref()) == email
                        && password_hash_digest(u.password_hash.as_deref()) == pwh
                        && u.mfa_enabled == expect_mfa
                }) else {
                    return Err(expired());
                };
                if db.sso().find(PROVIDER, &sub)?.is_some_and(|h| h.user_id != u.id) {
                    return Err(fail("sso_already_linked"));
                }
                match db.sso().link(u.id, PROVIDER, &sub, Some(&email), now) {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::SsoTaken => return Err(fail("sso_already_linked")),
                    Err(e) => return Err(e.into()),
                }
                if !u.email_verified {
                    db.users()
                        .update(u.id, &UserUpdate { email_verified: Some(true), ..UserUpdate::default() })?;
                }
                Ok::<_, AuthError>(u)
            })
            .await
            .map_err(|e| if e.is_store_busy() { AuthError::server_busy(Some(1)) } else { e })?;
        let method = if expect_mfa { "password+totp" } else { "password" };
        self.events.record(
            "sso_linked",
            Some(user.id),
            ip,
            Some(json!({ "provider": PROVIDER, "method": method })),
        );
        self.mail(
            Template::SsoLinked { username: &linked.username, when_ms: self.now() },
            linked.email.as_deref(),
        );
        Ok(())
    }

    /// `POST /auth/sso/google/link`: the account's password, typed in the game, before its link.
    pub(crate) async fn sso_link(&self, p: &SsoLinkParams) -> AuthResult<Value> {
        self.require_sso()?;
        let ip = p.ip.as_deref();
        if !is_prefixed_token(&p.link_ticket, SSO_PREFIX) {
            return Err(expired());
        }
        let h = sha256_hex(&p.link_ticket);
        let row = self.store.tokens().get(SSO_LINK.into(), h.clone()).await?;
        let Some(row) = row.filter(|r| is_live(Some(r), self.now())) else { return Err(expired()) };
        let d = data_of(&row);
        let user = match d.get("userId").and_then(Value::as_u64).and_then(|id| UserId::try_from(id).ok()) {
            Some(id) => self.store.users().by_id(id).await?,
            None => None,
        };
        let Some(user) = user.filter(|u| u.status == UserStatus::Active) else {
            self.store.tokens().consume(SSO_LINK.into(), h, self.now()).await?;
            return Err(expired());
        };
        // One of the ticket's tries, taken after the login's wait (429) and proof of work (428)
        // and before the hash: parallel requests cannot pass the limit.
        let tries = AtomicI64::new(0);
        let checked = self
            .verify_password(PasswordCheck {
                key: format!("l:{}", user.username.to_lowercase()),
                user: Some(&user),
                password: &p.password,
                ip,
                pow: p.pow.as_ref(),
                method: "google_link",
                reserve_link: Some((&h, &tries)),
            })
            .await;
        let current = match checked {
            Ok(u) => u,
            // The last try's wrong password ends the ticket; a wait, a proof of work or a refusal
            // of the hash queue keep it.
            Err(e) if e.code() == "invalid_credentials" && tries.load(Ordering::SeqCst) >= LINK_TRIES => {
                self.store.tokens().consume(SSO_LINK.into(), h, self.now()).await?;
                return Err(expired());
            }
            Err(e) => return Err(e),
        };
        if self.store.tokens().consume(SSO_LINK.into(), h, self.now()).await?.is_none() {
            return Err(expired());
        }
        let email = str_field(&d, "email").unwrap_or("");
        if current.status != UserStatus::Active
            || normalize_opt_email(current.email.as_deref()) != email
            || !usable_password(&current)
        {
            return Err(expired());
        }
        self.check_account_allowed(&current, true).await?;
        // The hash the password matched now (after a rehash): the MFA step and the link check it.
        let proven = ProvenLink {
            sub: str_field(&d, "sub").unwrap_or("").to_owned(),
            email: email.to_owned(),
            pwh: password_hash_digest(current.password_hash.as_deref()),
        };
        let client_label = p.client_label.as_deref();
        if current.mfa_enabled {
            let mut extra = Map::new();
            extra.insert("link".into(), proven.to_value());
            extra.insert("pwh".into(), proven.pwh.clone().into());
            return self.finish_login(&current, client_label, ip, PROVIDER, Some(extra)).await;
        }
        self.sso_link_proven(&current, &proven.to_value(), ip, false).await?;
        let checked = current.password_hash.clone();
        let fresh = self.store.users().by_id(current.id).await?.unwrap_or(current);
        let proven = checked.as_deref().map(|hash| ProvenPassword { hash, stale: expired });
        self.session_answer(&fresh, client_label, ip, "google+password", proven).await
    }

    /// `POST /auth/sso/complete`: creates the account of a first Google sign-in.
    pub(crate) async fn sso_complete(&self, p: &SsoCompleteParams) -> AuthResult<Value> {
        self.require_sso()?;
        let ip = p.ip.as_deref();
        if self.config.registration != Registration::Open {
            return Err(fail("registration_closed"));
        }
        check_username(&p.username, self.usernames)
            .map_err(|m| AuthError::new(400, "invalid_username", m))?;
        if !is_prefixed_token(&p.sso_ticket, SSO_PREFIX) {
            return Err(expired());
        }
        let h = sha256_hex(&p.sso_ticket);
        let row = self.store.tokens().get(SSO_TICKET.into(), h.clone()).await?;
        let Some(row) = row.filter(|r| is_live(Some(r), self.now())) else { return Err(expired()) };
        let d = data_of(&row);
        let (sub, email) =
            (str_field(&d, "sub").unwrap_or("").to_owned(), str_field(&d, "email").unwrap_or("").to_owned());
        // A username held by a pending signup of another address is taken as well.
        let (username, held_email, now) = (p.username.clone(), email.clone(), self.now());
        let taken = self
            .store
            .read(move |db| {
                Ok::<_, StoreError>(
                    db.users().by_username(&username)?.is_some()
                        || username_held(db, &username, &held_email, now)?,
                )
            })
            .await?;
        if taken {
            return Err(AuthError::new(409, "username_taken", "This username is already taken."));
        }
        if self.store.tokens().consume(SSO_TICKET.into(), h, self.now()).await?.is_none() {
            return Err(expired());
        }
        if self.store.sso().find(PROVIDER.into(), sub.clone()).await?.is_some() {
            return Err(AuthError::new(
                409,
                "sso_already_linked",
                "This Google account is already linked; sign in with Google again.",
            ));
        }
        let new_user = NewUser {
            username: p.username.clone(),
            email: Some(email.clone()),
            password_hash: None,
            email_verified: true,
            accept_challenges: true,
            created_at: self.now(),
        };
        let id = match self.store.users().create(new_user).await {
            Ok(id) => id,
            Err(e) if e.kind() == ErrorKind::UsernameTaken => {
                return Err(AuthError::new(
                    409,
                    "username_taken",
                    "This username was just taken; sign in with Google again and choose another one.",
                ));
            }
            Err(e) if e.kind() == ErrorKind::EmailTaken => {
                return Err(AuthError::new(
                    409,
                    "email_taken",
                    "An account with this e-mail address was just created; sign in with Google again to use it.",
                ));
            }
            Err(e) => return Err(e.into()),
        };
        self.store.sso().link(id, PROVIDER.into(), sub, Some(email), self.now()).await?;
        self.events.record("sso_account_created", Some(id), ip, Some(json!({ "provider": PROVIDER })));
        let Some(user) = self.store.users().by_id(id).await? else {
            return Err(AuthError::internal("the account just created is gone"));
        };
        self.mail(
            Template::SsoAccountCreated { username: &user.username, when_ms: self.now() },
            user.email.as_deref(),
        );
        self.session_answer(&user, p.client_label.as_deref(), ip, PROVIDER, None).await
    }
}
