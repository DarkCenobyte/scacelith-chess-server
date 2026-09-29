-- Engine analysis queue priority. Jobs are taken by priority, then oldest first: 0 ordinary,
-- 1 suspicion signal, 2 player report, 3 moderator request. Ordinary games that find the queue
-- full are not inserted at all (ANALYSIS_QUEUE_MAX).
ALTER TABLE analysis_jobs ADD COLUMN priority INTEGER NOT NULL DEFAULT 0;
DROP INDEX analysis_jobs_queue;
CREATE INDEX analysis_jobs_queue ON analysis_jobs (status, priority DESC, queued_at);
