-- FIDE ratings (src/match/elo.js) and the rating refunds of the victims of banned cheaters.
--
-- The unrated phase of a record (FIDE 8.2): `rated` is 0 until the first rating, while its games
-- add up the opponents' ratings and the score in half points. The records written before this
-- migration were rated under the previous scheme: those with games stay rated, only a record
-- without a game starts unrated.
ALTER TABLE ratings ADD COLUMN rated INTEGER NOT NULL DEFAULT 1;
ALTER TABLE ratings ADD COLUMN unrated_games INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ratings ADD COLUMN unrated_opponents INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ratings ADD COLUMN unrated_half_points INTEGER NOT NULL DEFAULT 0;
UPDATE ratings SET rated = 0 WHERE games = 0;

-- The development coefficient of each side's rating change (0: no K-formula change, the unrated
-- phase or an unrated opponent; NULL: a game finished before this migration, or not rated). A
-- refund gives back only a K-formula loss: the game that establishes a first rating moves it from
-- the working rating, which was no rating to lose.
ALTER TABLE games ADD COLUMN white_k INTEGER;
ALTER TABLE games ADD COLUMN black_k INTEGER;

-- Rating points given back to the opponents of a player banned for cheating (anticheat/refunds.js):
-- the points a victim lost in a rated game against the cheater, added to the victim's current
-- rating in that category. At most one refund per game and victim, whatever the number of bans.
-- notified_at: when Notice{RatingRestored} reached the victim (NULL: not yet; the primary sends it
-- when the victim is connected and not playing).
CREATE TABLE rating_refunds (
    id          INTEGER PRIMARY KEY,
    game_id     INTEGER NOT NULL REFERENCES games (id),
    victim_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    cheater_id  INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    category    TEXT    NOT NULL,
    points      INTEGER NOT NULL CHECK (points > 0),
    created_at  INTEGER NOT NULL,
    sanction_id INTEGER,                              -- the ban that triggered it (NULL: a moderator's later command)
    source      TEXT    NOT NULL CHECK (source IN ('auto', 'moderator')),
    created_by  TEXT,                                 -- the moderator
    notified_at INTEGER,
    UNIQUE (game_id, victim_id)
);
CREATE INDEX rating_refunds_cheater ON rating_refunds (cheater_id, id);
CREATE INDEX rating_refunds_victim ON rating_refunds (victim_id, id);
CREATE INDEX rating_refunds_pending ON rating_refunds (victim_id) WHERE notified_at IS NULL;
