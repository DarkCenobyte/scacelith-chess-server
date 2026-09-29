// In-memory stand-in for the parts of the Store API (DESIGN 5.5) the anti-cheat module uses.
// For unit tests and local experiments only; the real store is src/store/index.js.
//
// With { textColumns: true } it behaves like a store that binds free-form values straight to
// SQLite: passing an object for detail / evidence / features throws a
// TypeError, as node:sqlite does.

import { LEVELS } from '../util.js';

/**
 * @param {{ textColumns?: boolean }} [o]
 */
export function createFakeStore({ textColumns = false } = {}) {
    const users = new Map(), sessions = [], ratings = new Map(), games = new Map(), sanctions = [], anomalies = [];
    const security = [], jobs = new Map(), integrity = new Map(), population = new Map(), reports = [], recovery = new Map();
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
            update(id, fields) { rec('users.update', [id, fields]); Object.assign(users.get(Number(id)), fields); },
        },
        sessions: {
            create(s) { const id = seq++; sessions.push({ id, revokedAt: null, lastSeenAt: s.createdAt, ...s }); return id; },
            listForUser(userId) { return sessions.filter((s) => s.userId === userId); },
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
        games: { byId(id) { return games.get(Number(id)) || null; } },
        sanctions: {
            create(s) { const id = seq++; sanctions.push({ id, liftedAt: null, liftedBy: null, ...s }); return id; },
            activeBan(userId, now) {
                return sanctions.find((s) => s.userId === userId && s.kind === 'ban' && !s.liftedAt && s.startsAt <= now && (!s.endsAt || s.endsAt > now)) || null;
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
        },
        analysis: {
            next(limit, workerId, now) {
                const out = [];
                for (const j of jobs.values()) {
                    if (out.length >= limit) break;
                    if (j.status === 'queued') { j.status = 'running'; j.workerId = workerId; j.claimedAt = now; out.push({ gameId: j.gameId, attempts: ++j.attempts }); }
                }
                return out;
            },
            complete(gameId, features) { bind(features); const j = jobs.get(Number(gameId)) || { gameId }; j.status = 'done'; j.features = features; j.completedAt = Date.now(); jobs.set(Number(gameId), j); },
            fail(gameId, error) { const j = jobs.get(Number(gameId)); if (j) { j.status = 'failed'; j.error = error; } },
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
            listOpen(limit = 100) { return reports.filter((r) => !r.outcome).slice(0, limit); },
            forReported(userId) { return reports.filter((r) => r.reportedId === userId); },
            forReporter(userId) { return reports.filter((r) => r.reporterId === userId); },
            resolve(id, outcome, by, now) {
                const r = reports.find((x) => x.id === id && !x.outcome);
                if (!r) return false;
                r.outcome = outcome; r.resolvedBy = by; r.resolvedAt = now;
                return true;
            },
        },
        close() {},

        // ---- test helpers (not part of the Store API) ----
        _: {
            users, sessions, ratings, games, sanctions, anomalies, security, jobs, integrity, population, reports, recovery,
            addUser(username, extra = {}) { const id = store.users.create({ username, email: `${username}@example.test`, passwordHash: 'x', emailVerified: true }); Object.assign(users.get(id), extra); return id; },
            setRating(userId, category, fields) { ratings.set(`${userId}|${category}`, { rating: 1500, games: 0, wins: 0, draws: 0, losses: 0, peak: 1500, reachedSenior: false, ...fields }); },
            addGame(g) { games.set(Number(g.id), g); return g.id; },
            enqueue(gameId) { jobs.set(Number(gameId), { gameId: Number(gameId), status: 'queued', attempts: 0 }); },
        },
    };
    return store;
}
