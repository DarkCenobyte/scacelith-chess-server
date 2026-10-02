// Two-step verification (TOTP) of an account: second-factor checks and enrolment.
//
// Enrolment: setup (after password re-authentication) stores a *pending* secret (encrypted) and
// returns it with the otpauth:// URI; enable (a valid code of the pending secret) activates it and
// returns 10 recovery codes (shown once, stored as peppered HMACs); disable needs the password and
// a code or a recovery code; the recovery codes can be regenerated with the password and a code.
// A code is accepted once: the matched time step is stored atomically (advanceMfaStep).
//
// Every second factor checked (at sign-in and in account changes) first takes one token of the
// account's whole-server limit, AUTH_MFA_PER_ACCOUNT per 15 minutes (the primary's
// `ratelimit.take`, key "mfa:u<id>", one IPC per attempt): beyond it 429 too_many_attempts with
// retryAfter, before the code is looked at, so that no recovery code is spent. It bounds a code
// guesser who knows the password at that rate whatever the number of addresses or workers (the
// per-process failure delays of login.js and accounts.js come on top).

import { AuthError, tooManyAttempts } from './errors.js';
import {
    base32Encode, generateRecoveryCodes, generateTotpSecret, hashRecoveryCode, isTotpCode, normalizeRecoveryCode,
    otpauthUri, verifyTotp, TOTP_DIGITS, TOTP_PERIOD_S,
} from '../security/totp.js';

const aadActive = (userId) => `mfa:${userId}`;
const aadPending = (userId) => `mfa-pending:${userId}`;

/** Window of the per-account second-factor limit (AUTH_MFA_PER_ACCOUNT). */
export const MFA_LIMIT_WINDOW_MS = 15 * 60000;

/**
 * @param {{ config: object, store: object, now: () => number, box: object, keys: object, events: object,
 *   control: (type: string, payload: object) => Promise<object>, atomically: (fn: () => any) => any }} svc
 */
export function createMfa(svc) {
    const { config, store, now, box, keys, events, control } = svc;

    function hashCode(userId, code) {
        return hashRecoveryCode(keys.recovery, userId, normalizeRecoveryCode(code));
    }

    /**
     * Checks a second factor of `user` (a Store user row): a 6-digit TOTP code (in `code`), or a
     * recovery code (in `recoveryCode`, or in `code` when it has the recovery-code shape).
     * A TOTP code is consumed (its step stored), a recovery code is consumed.
     * @returns {Promise<boolean>}
     * @throws {AuthError} 429 too_many_attempts: the account's AUTH_MFA_PER_ACCOUNT is spent
     */
    async function checkSecondFactor(user, { code, recoveryCode, allowRecovery = true, ip = null }) {
        let totpCode = null, rc = null;
        if (typeof code === 'string' && code.trim()) {
            const c = code.trim().replace(/\s/g, '');
            if (isTotpCode(c)) totpCode = c;
            else rc = code;
        }
        if (!totpCode && typeof recoveryCode === 'string' && recoveryCode.trim()) rc = recoveryCode;
        if ((totpCode || (rc && allowRecovery)) && control && config.authMfaPerAccount) {
            const r = await control('ratelimit.take', { key: `mfa:u${user.id}`, limit: config.authMfaPerAccount, windowMs: MFA_LIMIT_WINDOW_MS, cost: 1 });
            if (r && r.allowed === false) throw tooManyAttempts(r.retryAfterMs || 1000);
        }
        if (totpCode) {
            const secret = user.mfaSecretEnc ? box.open(user.mfaSecretEnc, aadActive(user.id)) : null;
            if (!secret) return false;
            const step = verifyTotp(secret, totpCode, { now: now(), lastStep: user.mfaLastStep ?? -1 });
            secret.fill(0);
            if (step < 0) return false;
            return !!store.users.advanceMfaStep(user.id, step);
        }
        if (rc && allowRecovery) {
            if (!normalizeRecoveryCode(rc)) return false;
            const ok = !!store.mfa.consumeRecoveryCode(user.id, hashCode(user.id, rc));
            if (ok) {
                let remaining = null;
                try { remaining = store.mfa.countRecoveryCodes(user.id); } catch { /* informative only */ }
                events.record('recovery_code_used', { userId: user.id, ip, detail: { remaining } });
            }
            return ok;
        }
        return false;
    }

    /** Stores a new pending secret; returns what the authenticator app needs. */
    function setup(user) {
        if (user.mfaEnabled) throw new AuthError(409, 'mfa_already_enabled', 'Two-step verification is already enabled.');
        const secret = generateTotpSecret();
        store.users.update(user.id, { pendingMfaSecretEnc: box.seal(secret, aadPending(user.id)) });
        const out = {
            secret: base32Encode(secret),
            uri: otpauthUri({ issuer: config.serverName, account: user.username, secret }),
            algorithm: 'SHA1', digits: TOTP_DIGITS, period: TOTP_PERIOD_S,
        };
        secret.fill(0);
        return out;
    }

    /**
     * Activates the pending secret when `code` is valid for it.
     * @returns {{ ok: true, recoveryCodes: string[] } | { ok: false }}
     */
    function enable(user, code) {
        if (user.mfaEnabled) throw new AuthError(409, 'mfa_already_enabled', 'Two-step verification is already enabled.');
        const secret = user.pendingMfaSecretEnc ? box.open(user.pendingMfaSecretEnc, aadPending(user.id)) : null;
        if (!secret) throw new AuthError(409, 'mfa_setup_required', 'Start the two-step verification setup first.');
        const step = verifyTotp(secret, code, { now: now(), lastStep: -1 });
        if (step < 0) { secret.fill(0); return { ok: false }; }
        const codes = generateRecoveryCodes();
        const mfaSecretEnc = box.seal(secret, aadActive(user.id));
        secret.fill(0);
        // One transaction: MFA is never on without its recovery codes.
        svc.atomically(() => {
            store.users.update(user.id, { mfaEnabled: true, mfaSecretEnc, pendingMfaSecretEnc: null, mfaLastStep: step });
            store.mfa.replaceRecoveryCodes(user.id, codes.map((c) => hashCode(user.id, c)));
        });
        return { ok: true, recoveryCodes: codes };
    }

    /** Turns MFA off (the caller has checked the password and the second factor). */
    function disable(user) {
        svc.atomically(() => {
            store.users.update(user.id, { mfaEnabled: false, mfaSecretEnc: null, pendingMfaSecretEnc: null });
            store.mfa.replaceRecoveryCodes(user.id, []);
        });
    }

    /** New recovery codes (the old ones stop working). */
    function regenerate(user) {
        const codes = generateRecoveryCodes();
        store.mfa.replaceRecoveryCodes(user.id, codes.map((c) => hashCode(user.id, c)));
        return codes;
    }

    return { checkSecondFactor, setup, enable, disable, regenerate };
}
