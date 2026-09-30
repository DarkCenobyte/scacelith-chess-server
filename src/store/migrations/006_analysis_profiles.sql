-- Population statistics per analysis profile: the key becomes
-- '<profile>|<category>|<ratingBucket>|<metric>' (the profile names the engine, its network, the
-- depths, the hash size and the version of the analysis rules, src/anticheat/analysis/analyzer.js),
-- because games analysed differently are not comparable. The rows written before carry no profile
-- and nothing tells which engine or depths produced them, so they are deleted: the statistics start
-- again from the priors with the next games analysed. The players' levels and evidence are kept.
DELETE FROM population_stats WHERE key NOT LIKE '%|%|%|%';
