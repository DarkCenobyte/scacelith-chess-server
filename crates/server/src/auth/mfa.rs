//! Two-step verification (TOTP) of an account: second-factor checks and enrolment.
//!
//! Enrolment: the setup (after the password) stores a *pending* secret (encrypted) and returns it
//! with its `otpauth://` URI; the enable (a valid code of the pending secret) activates it and
//! returns 10 recovery codes (shown once, stored as peppered HMACs); the disable needs the password
//! and a code or a recovery code; the recovery codes can be regenerated with the password and a
//! code. A code is accepted once: the matched time step is stored atomically.
//!
//! Every second factor checked (at sign-in and in account changes) first takes one token of the
//! account's whole-server limit, `AUTH_MFA_PER_ACCOUNT` per 15 minutes (key `mfa:u<id>`): beyond
//! it 429 `too_many_attempts`, before the code is looked at, so that no recovery code is spent.

use serde_json::{Value, json};

use super::Inner;
use super::error::{AuthError, AuthResult};
use crate::security::encoding::{js_is_space, js_trim};
use crate::security::recovery::{RECOVERY_CODE_COUNT, generate_recovery_codes, hash_typed_recovery_code};
use crate::security::secret_box::{mfa_aad, mfa_pending_aad};
use crate::security::totp::{
    TOTP_DIGITS, TOTP_PERIOD_S, base32_encode, generate_totp_secret, is_totp_code, otpauth_uri, verify_totp,
};
use crate::store::{StoreError, User, UserUpdate};

/// Window of the per-account second-factor limit (`AUTH_MFA_PER_ACCOUNT`).
pub const MFA_LIMIT_WINDOW_MS: i64 = 15 * 60_000;

fn already_enabled() -> AuthError {
    AuthError::new(409, "mfa_already_enabled", "Two-step verification is already enabled.")
}

/// A second factor as typed: a TOTP code or a recovery code.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SecondFactor<'a> {
    /// A 6-digit code (or a recovery code typed in the code field).
    pub code: Option<&'a str>,
    /// A recovery code.
    pub recovery_code: Option<&'a str>,
}

impl SecondFactor<'_> {
    /// True when neither field holds anything (empty strings count as absent).
    pub fn is_empty(&self) -> bool {
        self.code.is_none_or(str::is_empty) && self.recovery_code.is_none_or(str::is_empty)
    }
}

impl Inner {
    /// The stored hashes of new recovery codes of `user_id` (as shown: `xxxx-xxxx-xx`).
    fn recovery_hashes(&self, user_id: u32, codes: &[String]) -> Vec<String> {
        codes
            .iter()
            .filter_map(|c| hash_typed_recovery_code(self.keys.recovery.as_bytes(), i64::from(user_id), c))
            .collect()
    }

    /// Checks a second factor of `user`: a 6-digit TOTP code (in `code`), or a recovery code (in
    /// `recovery_code`, or in `code` when it is not a 6-digit code) when `allow_recovery`. A TOTP
    /// code is used up (its step stored), a recovery code is deleted. Errors: 429
    /// `too_many_attempts` when the account's `AUTH_MFA_PER_ACCOUNT` is spent.
    pub(crate) async fn check_second_factor(
        &self,
        user: &User,
        factor: SecondFactor<'_>,
        allow_recovery: bool,
        ip: Option<&str>,
    ) -> AuthResult<bool> {
        let mut totp_code: Option<String> = None;
        let mut rc: Option<&str> = None;
        if let Some(code) = factor.code.filter(|c| !js_trim(c).is_empty()) {
            let compact: String = js_trim(code).chars().filter(|&c| !js_is_space(c)).collect();
            if is_totp_code(&compact) {
                totp_code = Some(compact);
            } else {
                rc = Some(code);
            }
        }
        if totp_code.is_none()
            && let Some(r) = factor.recovery_code.filter(|r| !js_trim(r).is_empty())
        {
            rc = Some(r);
        }
        let limit = u32::try_from(self.config.auth_mfa_per_account.max(0)).unwrap_or(u32::MAX);
        if (totp_code.is_some() || (rc.is_some() && allow_recovery)) && limit > 0 {
            let take = self.control.take(&format!("mfa:u{}", user.id), limit, MFA_LIMIT_WINDOW_MS, 1);
            if !take.allowed {
                let wait = if take.retry_after_ms > 0 { take.retry_after_ms } else { 1000 };
                return Err(AuthError::too_many_attempts(wait));
            }
        }
        if let Some(code) = totp_code {
            let secret = user
                .mfa_secret_enc
                .as_deref()
                .and_then(|s| self.secret_box.open(s, &mfa_aad(user.id.into())));
            let Some(secret) = secret else { return Ok(false) };
            let Some(step) = verify_totp(&secret, &code, self.now(), user.mfa_last_step) else {
                return Ok(false);
            };
            return Ok(self.store.users().advance_mfa_step(user.id, step).await?);
        }
        if let Some(rc) = rc.filter(|_| allow_recovery) {
            let Some(hash) = hash_typed_recovery_code(self.keys.recovery.as_bytes(), user.id.into(), rc)
            else {
                return Ok(false);
            };
            let id = user.id;
            let (ok, remaining) = self
                .store
                .write(move |db| {
                    let ok = db.mfa().consume_recovery_code(id, &hash)?;
                    let remaining = if ok { db.mfa().count_recovery_codes(id).ok() } else { None };
                    Ok::<_, StoreError>((ok, remaining))
                })
                .await?;
            if ok {
                self.events.record(
                    "recovery_code_used",
                    Some(id),
                    ip,
                    Some(json!({ "remaining": remaining })),
                );
            }
            return Ok(ok);
        }
        Ok(false)
    }

    /// Stores a new pending secret for `user`; returns what the authenticator app needs:
    /// `{secret, uri, algorithm, digits, period}`.
    pub(crate) async fn mfa_setup_secret(&self, user: &User) -> AuthResult<Value> {
        if user.mfa_enabled {
            return Err(already_enabled());
        }
        let secret = generate_totp_secret();
        let sealed = self.secret_box.seal(secret.as_ref(), &mfa_pending_aad(user.id.into()));
        let update = UserUpdate { pending_mfa_secret_enc: Some(Some(sealed)), ..UserUpdate::default() };
        self.store.users().update(user.id, update).await?;
        Ok(json!({
            "secret": base32_encode(secret.as_ref()),
            "uri": otpauth_uri(&self.config.server_name, &user.username, secret.as_ref()),
            "algorithm": "SHA1",
            "digits": TOTP_DIGITS,
            "period": TOTP_PERIOD_S,
        }))
    }

    /// Activates the pending secret of `user` when `code` is valid for it: the recovery codes, or
    /// `None` for a wrong code.
    pub(crate) async fn mfa_activate(&self, user: &User, code: &str) -> AuthResult<Option<Vec<String>>> {
        if user.mfa_enabled {
            return Err(already_enabled());
        }
        let secret = user
            .pending_mfa_secret_enc
            .as_deref()
            .and_then(|s| self.secret_box.open(s, &mfa_pending_aad(user.id.into())));
        let Some(secret) = secret else {
            return Err(AuthError::new(
                409,
                "mfa_setup_required",
                "Start the two-step verification setup first.",
            ));
        };
        let Some(step) = verify_totp(&secret, code, self.now(), -1) else { return Ok(None) };
        let codes = generate_recovery_codes(RECOVERY_CODE_COUNT);
        let hashes = self.recovery_hashes(user.id, &codes);
        let sealed = self.secret_box.seal(&secret, &mfa_aad(user.id.into()));
        drop(secret);
        let (id, now) = (user.id, self.now());
        // One transaction: two-step verification is never on without its recovery codes.
        self.store
            .write(move |db| {
                db.users().update(
                    id,
                    &UserUpdate {
                        mfa_enabled: Some(true),
                        mfa_secret_enc: Some(Some(sealed)),
                        pending_mfa_secret_enc: Some(None),
                        mfa_last_step: Some(step),
                        ..UserUpdate::default()
                    },
                )?;
                db.mfa().replace_recovery_codes(id, &hashes, now)
            })
            .await?;
        Ok(Some(codes))
    }

    /// Turns two-step verification off (the caller checked the password and the second factor).
    pub(crate) async fn mfa_turn_off(&self, user: &User) -> AuthResult<()> {
        let (id, now) = (user.id, self.now());
        self.store
            .write(move |db| {
                db.users().update(
                    id,
                    &UserUpdate {
                        mfa_enabled: Some(false),
                        mfa_secret_enc: Some(None),
                        pending_mfa_secret_enc: Some(None),
                        ..UserUpdate::default()
                    },
                )?;
                db.mfa().replace_recovery_codes(id, &[], now)
            })
            .await?;
        Ok(())
    }

    /// New recovery codes for `user` (the old ones stop working).
    pub(crate) async fn mfa_new_recovery_codes(&self, user: &User) -> AuthResult<Vec<String>> {
        let codes = generate_recovery_codes(RECOVERY_CODE_COUNT);
        let hashes = self.recovery_hashes(user.id, &codes);
        self.store.mfa().replace_recovery_codes(user.id, hashes, self.now()).await?;
        Ok(codes)
    }
}
