-- Pending signups (src/auth/accounts.js, docs/DESIGN.md 8). With e-mail confirmation, an account is
-- created only when the link mailed to its address is used; until then the signup waits here and
-- holds its username for the life of the link, whether or not the address already has an account,
-- so that nothing tells which addresses are registered. token_hash (SHA-256 of the link's token) is
-- NULL when the address already has an account (no link: its owner got a notice instead); the link
-- of a new address is not mailed when one was mailed to it less than 5 minutes before. One row per
-- address: a new signup with the same address replaces it. The retention purge deletes the expired
-- rows, which frees their usernames.
CREATE TABLE pending_signups (
    id               INTEGER PRIMARY KEY,
    username         TEXT    NOT NULL,
    username_lower   TEXT    NOT NULL UNIQUE,
    email            TEXT    NOT NULL,
    email_normalized TEXT    NOT NULL UNIQUE,
    password_hash    TEXT    NOT NULL,
    token_hash       TEXT    UNIQUE,
    created_at       INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL
);
CREATE INDEX pending_signups_expiry ON pending_signups (expires_at);
