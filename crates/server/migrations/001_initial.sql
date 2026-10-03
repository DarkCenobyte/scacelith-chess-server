-- 001_initial: schema of the Scacelith dedicated server (Rust).
--
-- Conventions: every table is STRICT; times are Unix epoch milliseconds (INTEGER); booleans are
-- 0/1 INTEGERs; JSON documents are TEXT; token and code hashes are lowercase hex TEXT and the MFA
-- secrets are their sealed TEXT form, compared exactly as the auth module stores them. Columns
-- holding large blobs come last so that reading the others never loads their overflow pages.
-- Never edit this file once released: add NNN_<name>.sql instead (the checksum of every applied
-- migration is verified at start-up).

-- Server-wide values (key 'server_id': a random UUID created by the first migration run).
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT, WITHOUT ROWID;

-- Accounts. Rows are anonymized, never deleted (games and ratings keep their user ids), and ids
-- are never reused (AUTOINCREMENT).
CREATE TABLE users (
    id                     INTEGER PRIMARY KEY AUTOINCREMENT,
    username               TEXT    NOT NULL,
    username_lower         TEXT    NOT NULL UNIQUE,
    email                  TEXT,
    email_normalized       TEXT    UNIQUE,            -- trimmed, lower-case (Gmail dots kept)
    email_verified         INTEGER NOT NULL DEFAULT 0,
    password_hash          TEXT,                      -- NULL for SSO-only accounts
    mfa_enabled            INTEGER NOT NULL DEFAULT 0,
    mfa_last_step          INTEGER NOT NULL DEFAULT 0,
    status                 TEXT    NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deleted')),
    accept_challenges      INTEGER NOT NULL DEFAULT 1,
    created_at             INTEGER NOT NULL,
    last_login_at          INTEGER,
    deleted_at             INTEGER,
    mfa_secret_enc         TEXT,
    mfa_pending_secret_enc TEXT
) STRICT;

CREATE TABLE mfa_recovery_codes (
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash  TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, code_hash)
) STRICT, WITHOUT ROWID;

CREATE TABLE sessions (
    id              INTEGER PRIMARY KEY,
    user_id         INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash      TEXT    NOT NULL UNIQUE,
    created_at      INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL,
    idle_expires_at INTEGER NOT NULL,
    revoked_at      INTEGER,
    client_label    TEXT,
    ip              TEXT                              -- erased after RETENTION_IP_DAYS
) STRICT;
CREATE INDEX sessions_user ON sessions (user_id, created_at);
-- The retention purge must use this exact expression to search the index.
CREATE INDEX sessions_expiry ON sessions (min(expires_at, idle_expires_at));
CREATE INDEX sessions_revoked ON sessions (revoked_at) WHERE revoked_at IS NOT NULL;
CREATE INDEX sessions_ip ON sessions (created_at) WHERE ip IS NOT NULL;

-- Single-use tokens (e-mail verification, password reset, MFA login, SSO attempt...).
CREATE TABLE tokens (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL,
    token_hash  TEXT    NOT NULL,
    user_id     INTEGER REFERENCES users (id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    consumed_at INTEGER,
    data        TEXT,
    UNIQUE (kind, token_hash)
) STRICT;
CREATE INDEX tokens_expiry ON tokens (expires_at);
CREATE INDEX tokens_user ON tokens (user_id) WHERE user_id IS NOT NULL;

-- Signups waiting for the confirmation of their address. A row holds its username until it
-- expires; token_hash is NULL when the address already has an account (no link was mailed).
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
) STRICT;
CREATE INDEX pending_signups_expiry ON pending_signups (expires_at);

CREATE TABLE sso_identities (
    provider   TEXT    NOT NULL,
    subject    TEXT    NOT NULL,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    email      TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (provider, subject)
) STRICT, WITHOUT ROWID;
CREATE INDEX sso_identities_user ON sso_identities (user_id);

-- One rating record per player and official category (absent: initial rating, no game). `rated`
-- is 0 during the unrated phase, whose games add up the opponents' ratings and the score in half
-- points; counted_games are the games that entered the rating (K, provisional mark, leaderboard).
CREATE TABLE ratings (
    user_id             INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    category            TEXT    NOT NULL,
    rating              INTEGER NOT NULL,
    games               INTEGER NOT NULL DEFAULT 0,
    wins                INTEGER NOT NULL DEFAULT 0,
    draws               INTEGER NOT NULL DEFAULT 0,
    losses              INTEGER NOT NULL DEFAULT 0,
    peak                INTEGER NOT NULL,
    reached_senior      INTEGER NOT NULL DEFAULT 0,
    rated               INTEGER NOT NULL DEFAULT 0,
    counted_games       INTEGER NOT NULL DEFAULT 0,
    unrated_games       INTEGER NOT NULL DEFAULT 0,
    unrated_opponents   INTEGER NOT NULL DEFAULT 0,
    unrated_half_points INTEGER NOT NULL DEFAULT 0,
    updated_at          INTEGER NOT NULL,
    PRIMARY KEY (user_id, category)
) STRICT, WITHOUT ROWID;
CREATE INDEX ratings_board ON ratings (category, rating DESC, games DESC);

-- Finished games. id is the 53-bit game id (ids.rs: time-ordered, carries the host shard).
-- status: protocol GameStatus (1 white wins, 2 black wins, 3 draw, 4 aborted); reason: EndReason.
-- white_k / black_k: development coefficient of each side's rating change (0: no K-formula change;
-- NULL: not rated). flags: 1 rated requested, 2 recovered after a restart, 4 forfeit, 8 manual
-- clock press. moves: u16 LE per ply; spent / clocks: u32 LE per ply (charged time, mover's clock
-- after the move), NULL when not recorded.
CREATE TABLE games (
    id           INTEGER PRIMARY KEY,
    category     TEXT    NOT NULL,
    rated        INTEGER NOT NULL,
    base_ms      INTEGER NOT NULL,
    inc_ms       INTEGER NOT NULL,
    white_id     INTEGER NOT NULL REFERENCES users (id),
    black_id     INTEGER NOT NULL REFERENCES users (id),
    white_name   TEXT    NOT NULL,
    black_name   TEXT    NOT NULL,
    white_rating INTEGER,                             -- ratings shown at the start
    black_rating INTEGER,
    started_at   INTEGER NOT NULL,
    ended_at     INTEGER NOT NULL,
    status       INTEGER NOT NULL,
    reason       INTEGER NOT NULL,
    ply_count    INTEGER NOT NULL,
    white_before INTEGER,                             -- rating changes (NULL: not rated)
    white_after  INTEGER,
    black_before INTEGER,
    black_after  INTEGER,
    white_k      INTEGER,
    black_k      INTEGER,
    rematch_of   INTEGER,
    flags        INTEGER NOT NULL DEFAULT 0,
    moves        BLOB    NOT NULL,
    spent        BLOB,
    clocks       BLOB
) STRICT;
CREATE INDEX games_white ON games (white_id, id);
CREATE INDEX games_black ON games (black_id, id);

-- Conduct (abandon / abort / no-show) and the matchmaking cooldown derived from it.
CREATE TABLE conduct_events (
    id      INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind    TEXT    NOT NULL CHECK (kind IN ('abandon', 'abort', 'noshow')),
    at      INTEGER NOT NULL
) STRICT;
CREATE INDEX conduct_events_user ON conduct_events (user_id, at);
CREATE INDEX conduct_events_at ON conduct_events (at);

CREATE TABLE conduct_state (
    user_id        INTEGER PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    cooldown_until INTEGER NOT NULL DEFAULT 0,
    level          INTEGER NOT NULL DEFAULT 0,
    updated_at     INTEGER NOT NULL
) STRICT;

CREATE TABLE sanctions (
    id         INTEGER PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL CHECK (kind IN ('ban', 'mm_block', 'warning')),
    reason     TEXT,
    source     TEXT    NOT NULL CHECK (source IN ('auto', 'moderator')),
    game_id    INTEGER,
    starts_at  INTEGER NOT NULL,
    ends_at    INTEGER,                               -- NULL: permanent
    created_at INTEGER NOT NULL,
    created_by TEXT,
    lifted_at  INTEGER,
    lifted_by  TEXT
) STRICT;
CREATE INDEX sanctions_user ON sanctions (user_id, kind);

-- Protocol and analysis anomalies (no foreign keys: batched inserts must never fail on a vanished
-- user or game).
CREATE TABLE anomalies (
    id       INTEGER PRIMARY KEY,
    user_id  INTEGER,
    game_id  INTEGER,
    kind     TEXT    NOT NULL,
    severity TEXT    NOT NULL CHECK (severity IN ('info', 'suspicious', 'certain')),
    at       INTEGER NOT NULL,
    detail   TEXT
) STRICT;
CREATE INDEX anomalies_user ON anomalies (user_id, at);
CREATE INDEX anomalies_purge ON anomalies (at) WHERE severity <> 'certain';

CREATE TABLE security_events (
    id      INTEGER PRIMARY KEY,
    kind    TEXT    NOT NULL,
    user_id INTEGER,
    ip      TEXT,                                     -- erased after RETENTION_IP_DAYS
    at      INTEGER NOT NULL,
    detail  TEXT
) STRICT;
CREATE INDEX security_events_at ON security_events (at);
CREATE INDEX security_events_user ON security_events (user_id, at) WHERE user_id IS NOT NULL;
CREATE INDEX security_events_ip ON security_events (at) WHERE ip IS NOT NULL;

-- Engine analysis queue of finished rated games. priority: 0 ordinary, 1 suspicion signal,
-- 2 player report, 3 moderator request (highest first, then oldest). The two partial indexes hold
-- only the waiting signal jobs, counted per player (queries use the literals 'queued' and 1).
CREATE TABLE analysis_jobs (
    game_id     INTEGER PRIMARY KEY REFERENCES games (id) ON DELETE CASCADE,
    status      TEXT    NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'done', 'failed')),
    priority    INTEGER NOT NULL DEFAULT 0,
    attempts    INTEGER NOT NULL DEFAULT 0,
    worker      TEXT,
    queued_at   INTEGER NOT NULL,
    started_at  INTEGER,
    finished_at INTEGER,
    white_id    INTEGER,
    black_id    INTEGER,
    error       TEXT,
    features    TEXT
) STRICT;
CREATE INDEX analysis_jobs_queue ON analysis_jobs (status, priority DESC, queued_at);
CREATE INDEX analysis_jobs_signal_white ON analysis_jobs (white_id) WHERE status = 'queued' AND priority = 1;
CREATE INDEX analysis_jobs_signal_black ON analysis_jobs (black_id) WHERE status = 'queued' AND priority = 1;

CREATE TABLE player_integrity (
    user_id     INTEGER PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    level       TEXT    NOT NULL DEFAULT 'none' CHECK (level IN ('none', 'suspected', 'high_confidence', 'confirmed')),
    score       REAL    NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL,
    reviewed_by TEXT,
    reviewed_at INTEGER,
    note        TEXT,
    evidence    TEXT
) STRICT;
CREATE INDEX player_integrity_level ON player_integrity (level, score);

-- Running statistics (Welford: n, mean, sum of squared deviations) of the analysis features, key
-- '<profile>|<category>|<ratingBucket>|<metric>'.
CREATE TABLE population_stats (
    key        TEXT    PRIMARY KEY,
    n          INTEGER NOT NULL,
    mean       REAL    NOT NULL,
    m2         REAL    NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

-- Player reports (game_id 0: not about a particular game).
CREATE TABLE reports (
    id          INTEGER PRIMARY KEY,
    reporter_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    reported_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    game_id     INTEGER NOT NULL DEFAULT 0,
    category    TEXT    NOT NULL CHECK (category IN ('cheating', 'abuse', 'other')),
    weight      REAL    NOT NULL DEFAULT 1,
    status      TEXT    NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'actioned', 'dismissed')),
    created_at  INTEGER NOT NULL,
    resolved_at INTEGER,
    resolved_by TEXT,
    comment     TEXT,
    UNIQUE (reporter_id, reported_id, game_id)
) STRICT;
CREATE INDEX reports_open ON reports (created_at) WHERE status = 'open';
CREATE INDEX reports_reported ON reports (reported_id, created_at);
CREATE INDEX reports_reporter ON reports (reporter_id, created_at);

-- Rating points given back to the opponents of a player banned for cheating: the points a victim
-- lost in a rated game against the cheater, added to the victim's current rating in that
-- category. At most one refund per game and victim. sanction_id: the ban that triggered it (NULL:
-- a moderator's later command); notified_at: when the victim was told (NULL: not yet).
CREATE TABLE rating_refunds (
    id          INTEGER PRIMARY KEY,
    game_id     INTEGER NOT NULL REFERENCES games (id),
    victim_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    cheater_id  INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    category    TEXT    NOT NULL,
    points      INTEGER NOT NULL CHECK (points > 0),
    created_at  INTEGER NOT NULL,
    sanction_id INTEGER,
    source      TEXT    NOT NULL CHECK (source IN ('auto', 'moderator')),
    created_by  TEXT,
    notified_at INTEGER,
    UNIQUE (game_id, victim_id)
) STRICT;
CREATE INDEX rating_refunds_cheater ON rating_refunds (cheater_id, id);
CREATE INDEX rating_refunds_victim ON rating_refunds (victim_id, id);
CREATE INDEX rating_refunds_pending ON rating_refunds (victim_id) WHERE notified_at IS NULL;
