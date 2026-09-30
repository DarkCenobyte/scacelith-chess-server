-- The counted games of a rating record (src/match/elo.js): the games that entered the rating (those
-- of the unrated phase that counted, then those against rated opponents), which set K, the
-- provisional mark and a place on the leaderboard. The games played also include the games against
-- unrated opponents and the zero scores, which test no rating.
--
-- NULL: a record written before this column existed, read as all its games when it is rated (they
-- were all rated then) and as the games of its unrated phase otherwise. Every record the store
-- writes carries the count.
ALTER TABLE ratings ADD COLUMN counted_games INTEGER;
