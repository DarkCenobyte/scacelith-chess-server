-- 001_initial: schema of the Scacelith dedicated server.
--
-- Conventions: times are epoch milliseconds (INTEGER); booleans are 0/1; JSON is TEXT; token and
-- code hashes are opaque values (TEXT or BLOB, compared exactly as the auth module stores them).
-- Columns holding large blobs come last so that reading the others never loads their overflow
-- pages. Never edit this file once released: add NNN_<name>.sql instead (the checksum of every
-- applied migration is verified at start-up).

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

-- Accounts. Rows are anonymized, never deleted (games and ratings keep their user ids).
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
    mfa_secret_enc         BLOB,
    mfa_pending_secret_enc BLOB
);

CREATE TABLE mfa_recovery_codes (
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    code_hash  BLOB    NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, code_hash)
) WITHOUT ROWID;

CREATE TABLE sessions (
    id              INTEGER PRIMARY KEY,
    user_id         INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_hash      BLOB    NOT NULL UNIQUE,
    created_at      INTEGER NOT NULL,
    last_seen_at    INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL,
    idle_expires_at INTEGER NOT NULL,
    revoked_at      INTEGER,
    client_label    TEXT,
    ip              TEXT                              -- erased after RETENTION_IP_DAYS
);
CREATE INDEX sessions_user ON sessions (user_id, created_at);
CREATE INDEX sessions_expiry ON sessions (min(expires_at, idle_expires_at));
CREATE INDEX sessions_revoked ON sessions (revoked_at) WHERE revoked_at IS NOT NULL;
CREATE INDEX sessions_ip ON sessions (created_at) WHERE ip IS NOT NULL;

-- Single-use tokens (e-mail verification, password reset, MFA login, SSO attempt...).
CREATE TABLE tokens (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL,
    token_hash  BLOB    NOT NULL,
    user_id     INTEGER REFERENCES users (id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    consumed_at INTEGER,
    data        TEXT,
    UNIQUE (kind, token_hash)
);
CREATE INDEX tokens_expiry ON tokens (expires_at);
CREATE INDEX tokens_user ON tokens (user_id) WHERE user_id IS NOT NULL;

CREATE TABLE sso_identities (
    provider   TEXT    NOT NULL,
    subject    TEXT    NOT NULL,
    user_id    INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    email      TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (provider, subject)
) WITHOUT ROWID;
CREATE INDEX sso_identities_user ON sso_identities (user_id);

-- One Elo record per player and official category (absent = initial rating, no game).
CREATE TABLE ratings (
    user_id        INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    category       TEXT    NOT NULL,
    rating         INTEGER NOT NULL,
    games          INTEGER NOT NULL DEFAULT 0,
    wins           INTEGER NOT NULL DEFAULT 0,
    draws          INTEGER NOT NULL DEFAULT 0,
    losses         INTEGER NOT NULL DEFAULT 0,
    peak           INTEGER NOT NULL,
    reached_senior INTEGER NOT NULL DEFAULT 0,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (user_id, category)
) WITHOUT ROWID;
CREATE INDEX ratings_board ON ratings (category, rating DESC, games DESC);

-- Finished games. id is the 53-bit game id (src/util/ids.js: time-ordered).
-- moves: u16 LE per ply; spent / clocks: u32 LE per ply (charged time, mover's clock after it).
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
    status       INTEGER NOT NULL,                    -- protocol GameStatus
    reason       INTEGER NOT NULL,                    -- protocol EndReason
    ply_count    INTEGER NOT NULL,
    white_before INTEGER,                             -- rating changes (NULL: not rated)
    white_after  INTEGER,
    black_before INTEGER,
    black_after  INTEGER,
    rematch_of   INTEGER,
    flags        INTEGER NOT NULL DEFAULT 0,
    moves        BLOB,
    spent        BLOB,
    clocks       BLOB
);
CREATE INDEX games_white ON games (white_id, id);
CREATE INDEX games_black ON games (black_id, id);

-- Conduct (abandon / abort / no-show) and the matchmaking cooldown derived from it.
CREATE TABLE conduct_events (
    id      INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind    TEXT    NOT NULL CHECK (kind IN ('abandon', 'abort', 'noshow')),
    at      INTEGER NOT NULL
);
CREATE INDEX conduct_events_user ON conduct_events (user_id, at);
CREATE INDEX conduct_events_at ON conduct_events (at);

CREATE TABLE conduct_state (
    user_id        INTEGER PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    cooldown_until INTEGER NOT NULL DEFAULT 0,
    level          INTEGER NOT NULL DEFAULT 0,
    updated_at     INTEGER NOT NULL
);

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
);
CREATE INDEX sanctions_user ON sanctions (user_id, kind);

-- Protocol anomalies (no foreign keys: batched inserts must never fail on a vanished user).
CREATE TABLE anomalies (
    id       INTEGER PRIMARY KEY,
    user_id  INTEGER,
    game_id  INTEGER,
    kind     TEXT    NOT NULL,
    severity TEXT    NOT NULL CHECK (severity IN ('info', 'suspicious', 'certain')),
    at       INTEGER NOT NULL,
    detail   TEXT
);
CREATE INDEX anomalies_user ON anomalies (user_id, at);
CREATE INDEX anomalies_purge ON anomalies (at) WHERE severity <> 'certain';

CREATE TABLE security_events (
    id      INTEGER PRIMARY KEY,
    kind    TEXT    NOT NULL,
    user_id INTEGER,
    ip      TEXT,                                     -- erased after RETENTION_IP_DAYS
    at      INTEGER NOT NULL,
    detail  TEXT
);
CREATE INDEX security_events_at ON security_events (at);
CREATE INDEX security_events_user ON security_events (user_id, at) WHERE user_id IS NOT NULL;
CREATE INDEX security_events_ip ON security_events (at) WHERE ip IS NOT NULL;

-- Engine analysis queue of finished rated games (consumed by the analysis process).
CREATE TABLE analysis_jobs (
    game_id     INTEGER PRIMARY KEY REFERENCES games (id) ON DELETE CASCADE,
    status      TEXT    NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'done', 'failed')),
    attempts    INTEGER NOT NULL DEFAULT 0,
    worker      TEXT,
    queued_at   INTEGER NOT NULL,
    started_at  INTEGER,
    finished_at INTEGER,
    error       TEXT,
    features    TEXT
);
CREATE INDEX analysis_jobs_queue ON analysis_jobs (status, queued_at);

CREATE TABLE player_integrity (
    user_id     INTEGER PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    level       TEXT    NOT NULL DEFAULT 'none' CHECK (level IN ('none', 'suspected', 'high_confidence', 'confirmed')),
    score       REAL    NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL,
    reviewed_by TEXT,
    reviewed_at INTEGER,
    note        TEXT,
    evidence    TEXT
);
CREATE INDEX player_integrity_level ON player_integrity (level, score);

-- Running statistics (Welford: n, mean, sum of squared deviations) of the analysis features,
-- key '<category>|<ratingBucket>|<metric>'.
CREATE TABLE population_stats (
    key        TEXT    PRIMARY KEY,
    n          INTEGER NOT NULL,
    mean       REAL    NOT NULL,
    m2         REAL    NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;

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
);
CREATE INDEX reports_open ON reports (created_at) WHERE status = 'open';
CREATE INDEX reports_reported ON reports (reported_id, created_at);
CREATE INDEX reports_reporter ON reports (reporter_id, created_at);
