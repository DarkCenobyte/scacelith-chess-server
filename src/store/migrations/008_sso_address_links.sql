-- Google links made by e-mail address (src/auth/sso.js, docs/DESIGN.md 5.9). Before this version,
-- Google sign-in linked a Google identity to the password account that had the same address,
-- without its password. A Google link created more than 60 s after its password account was added
-- that way and may be a hijack (e-mail confirmation off, or a confirmation link clicked for someone
-- else's account): it is removed, and the sessions of those accounts are revoked. Their players
-- enter the password once in the game, which links Google again while the account's address is
-- the Google address; when it is not (it changed since the link, in the account or at Google),
-- Google sign-in offers a new account instead: they sign in with the password, change the
-- account's address to the Google one, then use Google. A link made when Google created the
-- account (within 60 s), the links of accounts without a usable password and other providers
-- stay. The attempts of the former browser-callback flow (sso_state, sso_attempt) are deleted.
-- A shard's session cache may still accept a revoked session for up to about 30 s.
UPDATE sessions SET revoked_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000
 WHERE revoked_at IS NULL AND user_id IN (
    SELECT s.user_id FROM sso_identities s JOIN users u ON u.id = s.user_id
     WHERE s.provider = 'google' AND u.password_hash IS NOT NULL AND u.password_hash NOT LIKE '!%'
       AND s.created_at > u.created_at + 60000);
DELETE FROM sso_identities
 WHERE provider = 'google'
   AND user_id IN (SELECT id FROM users WHERE password_hash IS NOT NULL AND password_hash NOT LIKE '!%')
   AND created_at > (SELECT u.created_at FROM users u WHERE u.id = sso_identities.user_id) + 60000;
DELETE FROM tokens WHERE kind IN ('sso_state', 'sso_attempt');
