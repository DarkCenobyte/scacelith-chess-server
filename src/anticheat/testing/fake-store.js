// In-memory stand-in for the parts of the Store API (DESIGN 5.5) the anti-cheat module uses.
// For unit tests and local experiments only; the real store is src/store/index.js.
//
// With { textColumns: true } it behaves like a store that binds free-form values straight to
// SQLite: passing an object for detail / evidence / features throws a
// TypeError, as node:sqlite does.

import { LEVELS } from '../util.js';

// AnalysisPriority and ANALYSIS_MAX_ATTEMPTS of src/store/index.js.
const PRIORITY = Object.freeze({ ordinary: 0, signal: 1, report: 2, manual: 3 });
const MAX_ATTEMPTS = 3;

// The GameStatus a result filter needs as White and as Black (games.listForUser of the real store).
const RESULT_STATUS = Object.freeze({ win: [1, 2], loss: [2, 1], draw: [3, 3] });

// A report as the real store returns it.
const asStored = ({ at, outcome, ...r }) => ({ ...r, createdAt: at, status: outcome ?? 'open' });

function gameOf(g, userId, { category = null, rated = null, result = null } = {}) {
    if (g.whiteId !== userId && g.blackId !== userId) return false;
    if (category !== null && g.category !== category) return false;
    if (rated !== null && !!g.rated !== !!rated) return false;
    return result === null || g.status === RESULT_STATUS[result]?.[g.whiteId === userId ? 0 : 1];
}

/**
 * @param {{ textColumns?: boolean }} [o]
 */
export function createFakeStore({ textColumns = false } = {}) {
    const users = new Map(), sessions = [], ratings = new Map(), games = new Map(), sanctions = [], anomalies = [];
    const security = [], jobs = new Map(), integrity = new Map(), population = new Map(), reports = [], recovery = new Map();
    const refunds = [];
    let seq = 1;
    const calls = [];
    const bind = (v) => {
        if (textColumns && v !== null && v !== undefined && typeof v === 'object') throw new TypeError('Provided value cannot be bound to SQLite parameter');
        return v;
    };
    const rec = (name, args) => calls.push({ name, args });

    const store = {
        calls,
        users: {
            create({ username, email, passwordHash, emailVerified }) {
                for (const u of users.values()) {
                    if (u.username.toLowerCase() === username.toLowerCase()) throw Object.assign(new Error('taken'), { code: 'username_taken' });
                }
                const id = seq++;
                users.set(id, { id, username, email, passwordHash, emailVerified: !!emailVerified, mfaEnabled: false, mfaSecretEnc: null,
                    mfaLastStep: 0, createdAt: Date.now(), lastLoginAt: null, status: 'active', acceptChallenges: true, pendingMfaSecretEnc: null });
                return id;
            },
            byId(id) { return users.get(Number(id)) || null; },
            byUsername(name) { for (const u of users.values()) if (u.username.toLowerCase() === String(name).toLowerCase()) return u; return null; },
            update(id, fields) {
                rec('users.update', [id, fields]);
                // Like the real store: the address of another account is refused (StoreError 'email_taken').
                const email = fields && typeof fields.email === 'string' ? fields.email.trim().toLowerCase() : null;
                for (const u of users.values()) {
                    if (email && u.id !== Number(id) && String(u.email ?? '').trim().toLowerCase() === email) throw Object.assign(new Error('taken'), { code: 'email_taken' });
                }
                Object.assign(users.get(Number(id)), fields);
            },
        },
        sessions: {
            create(s) { const id = seq++; sessions.push({ id, revokedAt: null, lastSeenAt: s.createdAt, ...s }); return id; },
            listForUser(userId) { return sessions.filter((s) => s.userId === userId); },
            allForUser(userId) { return sessions.filter((s) => s.userId === userId).sort((a, b) => (b.createdAt ?? 0) - (a.createdAt ?? 0) || b.id - a.id); },
            revokeAllForUser(userId) {
                const out = [];
                for (const s of sessions) if (s.userId === userId && !s.revokedAt) { s.revokedAt = Date.now(); out.push(s.tokenHash); }
                return out;
            },
        },
        mfa: { replaceRecoveryCodes(userId, hashes) { recovery.set(userId, [...hashes]); }, countRecoveryCodes(userId) { return (recovery.get(userId) || []).length; } },
        ratings: {
            get(userId, category) { return ratings.get(`${userId}|${category}`) || { rating: 1500, games: 0, wins: 0, draws: 0, losses: 0, peak: 1500, reachedSenior: false }; },
            forUser(userId) {
                const out = [];
                for (const [k, v] of ratings) if (k.startsWith(`${userId}|`)) out.push({ category: k.split('|')[1], ...v });
                return out;
            },
        },
        games: {
            byId(id) { return games.get(Number(id)) || null; },
            // The account history of the real store (filters: category, rated, result from the player's side).
            listForUser(userId, { before = null, limit = 20, ...filter } = {}) {
                return [...games.values()].filter((g) => gameOf(g, userId, filter) && (before === null || g.id < before))
                    .sort((a, b) => b.id - a.id).slice(0, limit);
            },
            countForUser(userId, filter = null) { return [...games.values()].filter((g) => gameOf(g, userId, filter || {})).length; },
        },
        sanctions: {
            create(s) { const id = seq++; sanctions.push({ id, liftedAt: null, liftedBy: null, ...s }); return id; },
            activeBan(userId, now) {
                return sanctions.find((s) => s.userId === userId && s.kind === 'ban' && !s.liftedAt && s.startsAt <= now && (!s.endsAt || s.endsAt > now)) || null;
            },
            active(userId, now) {
                return sanctions.filter((s) => s.userId === userId && !s.liftedAt && s.startsAt <= now && (!s.endsAt || s.endsAt > now));
            },
            list(userId) { return sanctions.filter((s) => s.userId === userId); },
            lift(id, by, now) { const s = sanctions.find((x) => x.id === id); if (s) { s.liftedAt = now; s.liftedBy = by; } },
        },
        anomalies: {
            insertBatch(rows) {
                rows.forEach((r) => bind(r.detail));
                rec('anomalies.insertBatch', [rows]);
                for (const r of rows) anomalies.push({ id: seq++, ...r });
            },
            forUser(userId, limit = 100) { return anomalies.filter((a) => a.userId === userId).sort((a, b) => b.at - a.at).slice(0, limit); },
        },
        security: {
            insertBatch(rows) { rows.forEach((r) => bind(r.detail)); for (const r of rows) security.push(r); },
            forUser(userId, limit = 100) { return security.filter((e) => e.userId === userId).sort((a, b) => (b.at ?? 0) - (a.at ?? 0)).slice(0, limit); },
        },
        analysis: {
            // Highest priority first, then the order of queueing (the real store's reserved
            // ordinary share is not modelled).
            next(limit, workerId, now) {
                const out = [];
                const queued = [...jobs.values()].filter((j) => j.status === 'queued').sort((a, b) => (b.priority ?? 0) - (a.priority ?? 0));
                for (const j of queued.slice(0, limit)) {
                    j.status = 'running'; j.workerId = workerId; j.claimedAt = now;
                    out.push({ gameId: j.gameId, attempts: ++j.attempts, priority: j.priority ?? 0 });
                }
                return out;
            },
            complete(gameId, features) { bind(features); const j = jobs.get(Number(gameId)) || { gameId }; j.status = 'done'; j.features = features; j.completedAt = Date.now(); jobs.set(Number(gameId), j); },
            // As the real store: re-queued until it was claimed MAX_ATTEMPTS times, then failed;
            // returns the new status (null for an unknown job).
            fail(gameId, error) {
                const j = jobs.get(Number(gameId));
                if (!j) return null;
                j.status = (j.attempts ?? 0) >= MAX_ATTEMPTS ? 'failed' : 'queued';
                j.error = error;
                return j.status;
            },
            // Game eligibility (rated, length...) is not modelled: any known game can be requested.
            request(gameId, reason = 'report') {
                rec('analysis.request', [gameId, reason]);
                const j = jobs.get(Number(gameId));
                if (!games.has(Number(gameId)) || (j && j.status !== 'queued' && j.status !== 'failed')) return false;
                const priority = Math.max(j?.priority ?? 0, PRIORITY[reason] ?? PRIORITY.report);
                jobs.set(Number(gameId), { ...(j || { gameId: Number(gameId), attempts: 0 }), status: 'queued', reason, priority });
                return true;
            },
            // The completed analyses only (forUser(userId, limit, { doneOnly: true }) of the real store).
            forUser(userId, limit = 30) {
                const out = [];
                for (const j of jobs.values()) {
                    if (j.status !== 'done') continue;
                    const g = games.get(Number(j.gameId));
                    const f = typeof j.features === 'string' ? JSON.parse(j.features) : j.features;
                    const ids = g ? [g.whiteId, g.blackId] : [f?.white?.userId, f?.black?.userId];
                    if (ids.includes(userId)) out.push({ gameId: j.gameId, features: j.features, completedAt: j.completedAt, endedAt: g?.endedAt ?? f?.endedAt ?? 0 });
                }
                return out.sort((a, b) => b.endedAt - a.endedAt || b.completedAt - a.completedAt).slice(0, limit);
            },
        },
        integrity: {
            get(userId) { return integrity.get(userId) || null; },
            set(userId, fields) {
                bind(fields.evidence);
                rec('integrity.set', [userId, fields]);
                const prev = integrity.get(userId) || { userId, level: 'none', score: 0, evidence: null, updatedAt: null, reviewedBy: null };
                integrity.set(userId, { ...prev, ...fields, userId });
            },
            listFlagged(minLevel, limit = 100) {
                const min = LEVELS.indexOf(minLevel);
                return [...integrity.values()].filter((r) => LEVELS.indexOf(r.level) >= min && r.level !== 'none').slice(0, limit);
            },
            // Same contract as the real store: running statistics per '<prefix>|<metric>', read
            // back per prefix, observations merged (Welford).
            populationStats(prefix) {
                const out = {};
                for (const [k, v] of population) if (k.startsWith(prefix + '|')) out[k.slice(prefix.length + 1)] = { ...v };
                return out;
            },
            updatePopulation(observations) {
                for (const o of Array.isArray(observations) ? observations : [observations]) {
                    const x = +o.value;
                    if (!Number.isFinite(x)) continue;
                    const cur = population.get(o.key) || { n: 0, mean: 0, m2: 0 };
                    const n = cur.n + 1, d = x - cur.mean, mean = cur.mean + d / n;
                    population.set(o.key, { n, mean, m2: cur.m2 + d * (x - mean) });
                }
            },
        },
        reports: {
            create(r) { const id = seq++; reports.push({ id, outcome: null, resolvedBy: null, resolvedAt: null, ...r }); return id; },
            countByReporterSince(reporterId, since) { return reports.filter((r) => r.reporterId === reporterId && r.at >= since).length; },
            exists(reporterId, reportedId, gameId) { return reports.some((r) => r.reporterId === reporterId && r.reportedId === reportedId && r.gameId === gameId); },
            // The rows of listOpen and forReported have the real store's shape: createdAt and status
            // ('open' until an outcome) in place of `at` and `outcome`.
            listOpen(limit = 100) { return reports.filter((r) => !r.outcome).slice(0, limit).map(asStored); },
            // As the real store: the newest `limit` reports.
            forReported(userId, limit = 200) {
                return reports.filter((r) => r.reportedId === userId).sort((a, b) => (b.at ?? 0) - (a.at ?? 0) || b.id - a.id).slice(0, limit).map(asStored);
            },
            // As the real store: newest first, with the reported name, createdAt and status ('open'
            // until an outcome); `outcome` is what the reporter weighting reads.
            forReporter(userId, limit = 500) {
                return reports.filter((r) => r.reporterId === userId).sort((a, b) => (b.at ?? 0) - (a.at ?? 0) || b.id - a.id).slice(0, limit)
                    .map((r) => ({ ...r, createdAt: r.at, status: r.outcome ?? 'open', reportedName: users.get(r.reportedId)?.username ?? null }));
            },
            resolve(id, outcome, by, now) {
                const r = reports.find((x) => x.id === id && !x.outcome);
                if (!r) return false;
                r.outcome = outcome; r.resolvedBy = by; r.resolvedAt = now;
                return true;
            },
            resolveOpenFor(reportedId, category, outcome, by, now) {
                const open = reports.filter((r) => r.reportedId === reportedId && r.category === category && !r.outcome);
                for (const r of open) { r.outcome = outcome; r.resolvedBy = by; r.resolvedAt = now; }
                return open.map((r) => r.id);
            },
        },
        // Same contract as the real store (games as store.games.byId returns them, with whiteK /
        // blackK for the K factor; the games of the ratings are not checked against the records).
        refunds: {
            applyForCheater({ cheaterId, since = 0, now = Date.now(), sanctionId = null, source = 'moderator', by = null }) {
                const out = [];
                for (const g of [...games.values()].sort((a, b) => a.id - b.id)) {
                    const side = g.whiteId === cheaterId ? 'black' : g.blackId === cheaterId ? 'white' : null;
                    if (!side || !g.rated || !g.ratingChanges || (g.endedAt ?? 0) < since) continue;
                    const victimId = side === 'white' ? g.whiteId : g.blackId;
                    const points = g.ratingChanges[side].before - g.ratingChanges[side].after;
                    const rec = ratings.get(`${victimId}|${g.category}`);
                    if (!(points > 0) || (side === 'white' ? g.whiteK : g.blackK) === 0 || !rec) continue;
                    if (refunds.some((r) => r.gameId === g.id && r.victimId === victimId)) continue;
                    rec.rating += points;
                    rec.peak = Math.max(rec.peak, rec.rating);
                    const row = { id: seq++, gameId: g.id, victimId, cheaterId, category: g.category, points, createdAt: now, sanctionId, source,
                        createdBy: by, notifiedAt: null };
                    refunds.push(row);
                    out.push({ id: row.id, gameId: g.id, victimId, category: g.category, points, endedAt: g.endedAt });
                }
                return out;
            },
            list({ cheaterId = null, victimId = null, limit = 100 } = {}) {
                const name = (id) => users.get(id)?.username ?? null;
                return refunds.filter((r) => (cheaterId === null || r.cheaterId === cheaterId) && (victimId === null || r.victimId === victimId))
                    .sort((a, b) => b.id - a.id).slice(0, limit).map((r) => ({ ...r, victimName: name(r.victimId), cheaterName: name(r.cheaterId) }));
            },
            pendingSince(afterId = 0, limit = 1000) {
                return refunds.filter((r) => r.notifiedAt === null && r.id > afterId).slice(0, limit)
                    .map((r) => ({ id: r.id, victimId: r.victimId, points: r.points }));
            },
            pendingFor(victimId) {
                const rows = refunds.filter((r) => r.victimId === victimId && r.notifiedAt === null);
                return { ids: rows.map((r) => r.id), points: rows.reduce((n, r) => n + r.points, 0) };
            },
            markNotified(ids, now = Date.now()) {
                let n = 0;
                for (const r of refunds) if (ids.includes(r.id) && r.notifiedAt === null) { r.notifiedAt = now; n++; }
                return n;
            },
        },
        close() {},

        // ---- test helpers (not part of the Store API) ----
        _: {
            users, sessions, ratings, games, sanctions, anomalies, security, jobs, integrity, population, reports, recovery, refunds,
            addUser(username, extra = {}) { const id = store.users.create({ username, email: `${username}@example.test`, passwordHash: 'x', emailVerified: true }); Object.assign(users.get(id), extra); return id; },
            setRating(userId, category, fields) { ratings.set(`${userId}|${category}`, { rating: 1500, games: 0, wins: 0, draws: 0, losses: 0, peak: 1500, reachedSenior: false, ...fields }); },
            addGame(g) { games.set(Number(g.id), g); return g.id; },
            /** priority: AnalysisPriority of the real store (0 ordinary .. 3 manual). */
            enqueue(gameId, priority = PRIORITY.ordinary) { jobs.set(Number(gameId), { gameId: Number(gameId), status: 'queued', attempts: 0, priority }); },
        },
    };
    return store;
}
