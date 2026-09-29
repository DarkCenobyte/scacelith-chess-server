-- The players of each analysis job, so that the signal jobs waiting for one player can be counted:
-- a flagged player's further games are not queued while 20 of their games wait. The two partial
-- indexes hold only the waiting signal jobs (priority 1).
ALTER TABLE analysis_jobs ADD COLUMN white_id INTEGER;
ALTER TABLE analysis_jobs ADD COLUMN black_id INTEGER;
UPDATE analysis_jobs SET white_id = (SELECT g.white_id FROM games g WHERE g.id = analysis_jobs.game_id),
    black_id = (SELECT g.black_id FROM games g WHERE g.id = analysis_jobs.game_id);
CREATE INDEX analysis_jobs_signal_white ON analysis_jobs (white_id) WHERE status = 'queued' AND priority = 1;
CREATE INDEX analysis_jobs_signal_black ON analysis_jobs (black_id) WHERE status = 'queued' AND priority = 1;
