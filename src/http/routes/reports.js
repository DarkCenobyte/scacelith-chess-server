// POST /api/v1/reports: a player reports the opponent of one of their recent games.
//
// Body: { gameId, reported (username), category: 'cheating' | 'abuse' | 'other', comment? (<= 500) }.
// Answers: 202 { status: 'received' } (accepted, or already reported: the same answer, which
// says nothing about the reported account), 400 invalid_request, 403 report_not_allowed (not
// an opponent of the reporter in a game that ended within 7 days), 429 report_limit
// (REPORTS_PER_DAY). Rules, weighting and caps: src/anticheat/reports.js.

import { handleReport } from '../../anticheat/reports.js';

/**
 * Registers the route on the API router (DESIGN 5.9).
 * @param {{ post: Function }} router
 * @param {{ store?: object, config?: object, log?: object, now?: () => number }} [deps]
 */
export function register(router, deps = {}) {
    router.post('/api/v1/reports', (ctx) => handleReport(ctx, deps), {
        auth: 'required',
        // Coarse per-client cap on requests; the per-day report quota is enforced by the handler.
        rate: { key: 'reports', limit: 30, windowMs: 3600000 },
    });
}
