// Administration commands (bin/admin.js): accounts, sanctions, integrity reviews, reports, rating
// refunds.
//
// Every handler takes a Store object (DESIGN 5.5), so the commands run against the database on
// the server host (no network) and are tested with a fake store. Every moderator action is
// audited: a security event 'moderator_action' { action, moderator, ... }, a security log line,
// and reviewed_by / created_by where the data model has one.

import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { performance } from 'node:perf_hooks';
import { DatabaseSync } from '../store/index.js';
import { readIntegrity, writeIntegrity, writeStructured, parseMaybeJson, levelRank, LEVELS, HOUR_MS, DAY_MS } from './util.js';
import { CheatBanReason, isCheatingBan, refundVictims, refundWindowStart, victimTotals } from './refunds.js';
import { reviewPriority, recentReportWeight } from './reports.js';
import { sideOf } from './scoring.js';

export class AdminError extends Error {}

export const USAGE = `Usage: scacelith-admin <command> [options]

Accounts
  user show <name>                            account, ratings, sanctions, integrity summary
  user ban <name> --hours N --reason TEXT     ban for anything but cheating, no refunds (applies at the
                                              next connection or game) [--revoke-sessions]
  user unban <name>                           lift the active bans
  user reset-mfa <name>                       disable TOTP, delete recovery codes, log out everywhere
  user verify-email <name>                    mark the e-mail address verified
  user revoke-sessions <name>                 log the account out everywhere

Integrity
  integrity list [--level suspected|high_confidence|confirmed] [--limit N]
  integrity show <name>                       evidence, per-game features, anomalies, reports
  integrity confirm <name> --reason TEXT [--hours N] [--keep-reports] [--refund-since DATE | --no-refund]
                                              level confirmed + ban (open cheating reports -> actioned) +
                                              rating refunds of the games since DATE (default:
                                              RATING_REFUND_DAYS before now) and of those recorded
                                              during the ban; --no-refund: none of them
  integrity clear <name> [--reason TEXT] [--dismiss-reports]

Rating refunds (the points the victims of a confirmed cheater lost to them, given back)
  refunds apply <name> [--since DATE]         refunds of a confirmed cheater's games since DATE (default:
                                              RATING_REFUND_DAYS before their latest ban for
                                              cheating); those already given are skipped
  refunds list [<name>] [--victim NAME] [--limit N]
                                              refunds of a cheater's games, received by a victim, or all

Reports and anomalies
  reports list [--limit N]                    open reports grouped by reported player, by priority
  reports resolve <id> actioned|dismissed
  anomalies <name> [--limit N]
  stats

Data
  backup <file> [--verify]                    consistent copy of the database (VACUUM INTO), safe while
                                              the server runs; the file must not exist; mode 600

Test servers only
  bench-accounts --count N [--prefix bench] --out FILE [--format tokens|tsv] --i-know-this-is-a-test-server

Common options: --json (machine-readable output), --by NAME (moderator name; default: OS user)
`;

const BOOLEAN_FLAGS = new Set(['json', 'help', 'revoke-sessions', 'keep-reports', 'dismiss-reports', 'i-know-this-is-a-test-server', 'verify',
    'no-refund']);

/**
 * Parses command-line arguments: positionals, --flag value, --flag=value, boolean flags.
 * @param {string[]} argv
 * @returns {{ positional: string[], flags: Record<string, string|boolean> }}
 */
export function parseArgs(argv) {
    const positional = [], flags = {};
    for (let i = 0; i < argv.length; i++) {
        const a = argv[i];
        if (a.startsWith('--')) {
            const eq = a.indexOf('=');
            if (eq > 0) { flags[a.slice(2, eq)] = a.slice(eq + 1); continue; }
            const name = a.slice(2);
            if (BOOLEAN_FLAGS.has(name) || i + 1 >= argv.length || argv[i + 1].startsWith('--')) flags[name] = true;
            else flags[name] = argv[++i];
        } else positional.push(a);
    }
    return { positional, flags };
}

function iso(t) { return t ? new Date(Number(t)).toISOString().replace('.000Z', 'Z') : '-'; }

function table(rows, cols) {
    if (!rows.length) return '(none)\n';
    const w = cols.map((c) => Math.max(c.length, ...rows.map((r) => String(r[c] ?? '-').length)));
    const line = (vals) => vals.map((v, i) => String(v ?? '-').padEnd(w[i])).join('  ').trimEnd();
    return [line(cols), line(w.map((n) => '-'.repeat(n))), ...rows.map((r) => line(cols.map((c) => r[c])))].join('\n') + '\n';
}

function publicUser(u) {
    return { id: u.id, username: u.username, email: u.email, emailVerified: !!u.emailVerified, mfaEnabled: !!u.mfaEnabled,
        status: u.status, createdAt: u.createdAt, lastLoginAt: u.lastLoginAt, acceptChallenges: u.acceptChallenges };
}

function requireUser(ctx, name) {
    if (!name) throw new AdminError('a user name is required');
    const u = ctx.store.users.byUsername(name);
    if (!u) throw new AdminError(`no user named "${name}"`);
    return u;
}

function intFlag(ctx, name, { min, max, def } = {}) {
    const v = ctx.args.flags[name];
    if (v === undefined || v === true) {
        if (def !== undefined) return def;
        throw new AdminError(`--${name} N is required`);
    }
    if (!/^\d+$/.test(String(v))) throw new AdminError(`--${name} expects an integer`);
    const n = Number(v);
    if ((min !== undefined && n < min) || (max !== undefined && n > max)) throw new AdminError(`--${name} must be between ${min} and ${max}`);
    return n;
}

function textFlag(ctx, name, { required = false, max = 300 } = {}) {
    const v = ctx.args.flags[name];
    if (v === undefined || v === true || !String(v).trim()) {
        if (required) throw new AdminError(`--${name} TEXT is required`);
        return '';
    }
    const s = String(v).trim();
    if (s.length > max) throw new AdminError(`--${name} is limited to ${max} characters`);
    return s;
}

// A date option: YYYY-MM-DD (00:00 UTC) or an ISO 8601 time with its offset (2026-05-01T18:30Z),
// not in the future. The typed calendar date must exist: Date.parse rolls a day past the end of
// the month over (2026-02-31 would be 2026-03-03).
function dateFlag(ctx, name) {
    const v = ctx.args.flags[name];
    if (v === undefined) return null;
    const s = String(v).trim();
    const valid = /^\d{4}-\d{2}-\d{2}$/.test(s) || /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2}(\.\d+)?)?(Z|[+-]\d{2}:\d{2})$/.test(s);
    const [y, mo, d] = s.slice(0, 10).split('-').map(Number);
    const exists = mo >= 1 && mo <= 12 && d >= 1 && d <= new Date(Date.UTC(y, mo, 0)).getUTCDate();
    const t = valid && exists ? Date.parse(s) : NaN;
    if (!Number.isFinite(t)) throw new AdminError(`--${name} expects a date: YYYY-MM-DD (UTC) or an ISO 8601 time with its offset`);
    if (t > ctx.now()) throw new AdminError(`--${name} is in the future`);
    return t;
}

// Audit trail of a moderator action.
function audit(ctx, action, userId, detail = {}) {
    const d = { action, moderator: ctx.moderator, ...detail };
    ctx.log?.security?.('moderator.action', { userId, ...d });
    writeStructured((r) => ctx.store.security.insertBatch(r), [{ kind: 'moderator_action', userId: userId ?? null, ip: null, detail: d, at: ctx.now() }], ['detail']);
}

function reportsAgainst(ctx, userId) {
    try { return ctx.store.reports.forReported(userId) || []; } catch { return []; }
}

function isOpen(r) { return !(r.outcome ?? r.resolution ?? r.resolvedAt ?? r.resolved_at); }

function resolveOpenCheatingReports(ctx, userId, outcome) {
    let n = 0;
    for (const r of reportsAgainst(ctx, userId)) {
        if (!isOpen(r) || r.category !== 'cheating') continue;
        ctx.store.reports.resolve(r.id, outcome, ctx.moderator, ctx.now());
        n++;
    }
    return n;
}

function pushReview(ev, entry) {
    ev.reviews = [...(Array.isArray(ev.reviews) ? ev.reviews : []), entry].slice(-20);
}

// ---- user ------------------------------------------------------------------------------------------

function userShow(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    const s = ctx.store, now = ctx.now();
    const ratings = safe(() => s.ratings.forUser(u.id), []);
    const sanctions = safe(() => s.sanctions.list(u.id), []);
    const activeBan = safe(() => s.sanctions.activeBan(u.id, now), null);
    const integ = readIntegrity(s, u.id);
    const sessions = safe(() => s.sessions.listForUser(u.id), []).filter((x) => !x.revokedAt && (!x.expiresAt || x.expiresAt > now)
        && (!x.idleExpiresAt || x.idleExpiresAt > now)).length;
    const anomalies = { info: 0, suspicious: 0, certain: 0 };
    for (const a of safe(() => s.anomalies.forUser(u.id, 1000), [])) if (a.severity in anomalies) anomalies[a.severity]++;
    const rep = reportsAgainst(ctx, u.id);
    const data = {
        user: publicUser(u), ratings, activeBan, sanctions,
        integrity: { level: integ.level, score: integ.score, updatedAt: integ.updatedAt, reviewedBy: integ.reviewedBy },
        activeSessions: sessions, anomalies,
        reports: { total: rep.length, open: rep.filter(isOpen).length, weight30d: recentReportWeight(rep, now, 30) },
    };
    let t = `User ${u.username} (#${u.id})  ${u.status || 'active'}\n`;
    t += `  e-mail ${u.email || '-'} (${u.emailVerified ? 'verified' : 'NOT verified'}), MFA ${u.mfaEnabled ? 'on' : 'off'}\n`;
    t += `  created ${iso(u.createdAt)}, last login ${iso(u.lastLoginAt)}, active sessions ${sessions}\n`;
    t += `  integrity ${integ.level} (score ${integ.score.toFixed(2)})${integ.reviewedBy ? `, reviewed by ${integ.reviewedBy}` : ''}\n`;
    t += `  anomalies: ${anomalies.certain} certain, ${anomalies.suspicious} suspicious, ${anomalies.info} info; reports received: ${data.reports.total} (${data.reports.open} open)\n`;
    t += activeBan ? `  BANNED until ${iso(activeBan.endsAt)}: ${activeBan.reason}\n` : '  not banned\n';
    t += '\nRatings\n' + table(ratings.map((r) => ({ category: r.category, rating: r.rating, games: r.games, 'w/d/l': `${r.wins ?? 0}/${r.draws ?? 0}/${r.losses ?? 0}`, peak: r.peak })), ['category', 'rating', 'games', 'w/d/l', 'peak']);
    t += '\nSanctions\n' + table(sanctions.map((x) => ({ id: x.id, kind: x.kind, source: x.source, from: iso(x.startsAt), until: iso(x.endsAt), reason: x.reason, by: x.createdBy, lifted: x.liftedAt ? iso(x.liftedAt) : '' })), ['id', 'kind', 'source', 'from', 'until', 'reason', 'by', 'lifted']);
    return { data, text: t };
}

function userBan(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    const hours = intFlag(ctx, 'hours', { min: 1, max: 87600 });
    const reason = textFlag(ctx, 'reason', { required: true });
    // The reason of a ban tells whether it was given for cheating (refunds.js CheatBanReason):
    // this ban is not, and refunds nothing.
    if (isCheatingBan({ kind: 'ban', source: 'moderator', reason })) {
        throw new AdminError(`a reason starting with "${CheatBanReason.confirmed}" or "${CheatBanReason.confirmedNoRefund}" marks a ban for cheating: `
            + 'use integrity confirm for one, or word the reason differently');
    }
    const now = ctx.now(), until = now + hours * HOUR_MS;
    const id = ctx.store.sanctions.create({ userId: u.id, kind: 'ban', reason, source: 'moderator', gameId: null, startsAt: now, endsAt: until, createdBy: ctx.moderator });
    let revoked = 0;
    if (ctx.args.flags['revoke-sessions']) revoked = (ctx.store.sessions.revokeAllForUser(u.id) || []).length;
    audit(ctx, 'ban', u.id, { hours, reason, sanctionId: id, until, revokedSessions: revoked });
    return { data: { sanctionId: id, until, revokedSessions: revoked },
        text: `Banned ${u.username} until ${iso(until)} (sanction #${id}).${revoked ? ` ${revoked} sessions revoked.` : ''}\nThe running server applies it when the player next connects or tries to start a game (use --revoke-sessions to also log them out).\n` };
}

function userUnban(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    const now = ctx.now(), lifted = [];
    for (let i = 0; i < 50; i++) {
        const b = ctx.store.sanctions.activeBan(u.id, now);
        if (!b || lifted.includes(b.id)) break;
        ctx.store.sanctions.lift(b.id, ctx.moderator, now);
        lifted.push(b.id);
    }
    if (lifted.length) audit(ctx, 'unban', u.id, { sanctions: lifted });
    return { data: { lifted }, text: lifted.length ? `Lifted ${lifted.length} ban(s) of ${u.username}: #${lifted.join(', #')}.\n` : `${u.username} has no active ban.\n` };
}

function userResetMfa(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    ctx.store.users.update(u.id, { mfaEnabled: false, mfaSecretEnc: null, pendingMfaSecretEnc: null, mfaLastStep: 0 });
    ctx.store.mfa.replaceRecoveryCodes(u.id, []);
    const revoked = (ctx.store.sessions.revokeAllForUser(u.id) || []).length;
    audit(ctx, 'reset_mfa', u.id, { revokedSessions: revoked });
    return { data: { revokedSessions: revoked }, text: `MFA disabled for ${u.username}, recovery codes deleted, ${revoked} sessions revoked.\nOnly do this after verifying the owner by other means.\n` };
}

function userVerifyEmail(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    ctx.store.users.update(u.id, { emailVerified: true });
    audit(ctx, 'verify_email', u.id, {});
    return { data: { ok: true }, text: `E-mail address of ${u.username} marked verified.\n` };
}

function userRevokeSessions(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    const revoked = (ctx.store.sessions.revokeAllForUser(u.id) || []).length;
    audit(ctx, 'revoke_sessions', u.id, { revokedSessions: revoked });
    return { data: { revokedSessions: revoked }, text: `${revoked} session(s) of ${u.username} revoked (the servers' caches drop them within 30 s).\n` };
}

// ---- integrity ---------------------------------------------------------------------------------------

function integrityList(ctx) {
    const level = ctx.args.flags.level === undefined ? 'suspected' : String(ctx.args.flags.level);
    if (!LEVELS.includes(level) || level === 'none') throw new AdminError('--level must be suspected, high_confidence or confirmed');
    const limit = intFlag(ctx, 'limit', { min: 1, max: 10000, def: 50 });
    const now = ctx.now();
    const rows = (ctx.store.integrity.listFlagged(level, limit) || []).map((r) => {
        const userId = r.userId ?? r.user_id;
        const u = safe(() => ctx.store.users.byId(userId), null);
        const w = recentReportWeight(reportsAgainst(ctx, userId), now, 30);
        const ev = parseMaybeJson(r.evidence, {}) || {};
        return {
            userId, username: u?.username ?? `#${userId}`, level: r.level, score: Number(r.score) || 0,
            priority: reviewPriority({ level: r.level, score: r.score, reportWeight: w }), reports30d: w,
            games: ev.statistics?.windows?.all?.games ?? '-', updatedAt: r.updatedAt, reviewedBy: r.reviewedBy ?? null,
        };
    }).filter((r) => levelRank(r.level) >= levelRank(level)).sort((a, b) => b.priority - a.priority || b.score - a.score);
    const text = table(rows.map((r) => ({ ...r, score: r.score.toFixed(2), updated: iso(r.updatedAt), reviewed: r.reviewedBy || '' })),
        ['priority', 'username', 'level', 'score', 'games', 'reports30d', 'updated', 'reviewed']);
    return { data: rows, text };
}

function integrityShow(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    const s = ctx.store, now = ctx.now();
    const integ = readIntegrity(s, u.id);
    const ev = integ.evidence || {};
    const games = [];
    for (const row of safe(() => s.analysis.forUser(u.id, 30), [])) {
        const g = sideOf(parseMaybeJson(row?.features ?? row, null), u.id);
        if (g) games.push(g);
    }
    const anomalies = safe(() => s.anomalies.forUser(u.id, 50), []).map((a) => ({ ...a, detail: parseMaybeJson(a.detail, a.detail) }));
    const reports = reportsAgainst(ctx, u.id);
    const sanctions = safe(() => s.sanctions.list(u.id), []);
    const priority = reviewPriority({ level: integ.level, score: integ.score, reportWeight: recentReportWeight(reports, now, 30) });
    const data = { user: publicUser(u), integrity: integ, priority, games, anomalies, reports, sanctions };
    const st = ev.statistics;
    let t = `Integrity of ${u.username} (#${u.id}): ${integ.level}, score ${integ.score.toFixed(2)}, review priority ${priority}\n`;
    t += `  updated ${iso(integ.updatedAt)}${integ.reviewedBy ? `, last reviewed by ${integ.reviewedBy}` : ''}\n`;
    if (st) {
        t += `\nStatistical evidence (model v${st.model}, ${iso(st.computedAt)}): automatic level ${st.level}${st.trigger ? ` (${st.trigger})` : ''}\n`;
        if (st.profile) t += `  analysis profile: ${st.profile}\n`;
        t += `  groups: Q ${st.groups?.Q} (quality), E ${st.groups?.E} (engine choice), J ${st.groups?.J} (jump), T ${st.groups?.T} (timing)\n`;
        for (const r of st.reasons || []) t += `  - ${r}\n`;
        if (ev.peak && ev.peak.score > st.score) t += `  peak score ${ev.peak.score} on ${iso(ev.peak.at)} (level ${ev.peak.level})\n`;
    } else t += '\nNo statistical evidence yet.\n';
    if (Array.isArray(ev.certain) && ev.certain.length) {
        t += '\nCertain protocol cheats\n' + table(ev.certain.map((c) => ({ at: iso(c.at), kind: c.kind, game: c.gameId, banUntil: iso(c.banUntil) })), ['at', 'kind', 'game', 'banUntil']);
    }
    if (Array.isArray(ev.reviews) && ev.reviews.length) {
        t += '\nReviews\n' + table(ev.reviews.map((r) => ({ at: iso(r.at), action: r.action, by: r.by, reason: r.reason || '' })), ['at', 'action', 'by', 'reason']);
    }
    const f = (x, d = 1) => (x === null || x === undefined ? '-' : Number(x).toFixed(d));
    // A side without scored moves has no rates (null): '-', not 0 %.
    const pct = (x) => (x === null || x === undefined ? '-' : f(x * 100, 0));
    // Games of another analysis profile than the statistics' are not part of the scores.
    const other = (g) => !!st?.profile && g.profile !== st.profile;
    t += '\nAnalysed games (newest first)\n' + table(games.map((g) => ({
        game: other(g) ? `${g.gameId}*` : g.gameId, cat: g.category, rating: g.rating, moves: g.n, acc: f(g.accuracy), acpl: f(g.acpl),
        't1%': pct(g.t1Deep), 'fast%': pct(g.t1Fast), 'cx%': g.t1Complex === null ? '-' : `${pct(g.t1Complex)}/${g.nComplex}`,
        'time~cx': f(g.timeCorr, 2), cv: f(g.timeCv, 2),
    })), ['game', 'cat', 'rating', 'moves', 'acc', 'acpl', 't1%', 'fast%', 'cx%', 'time~cx', 'cv']);
    if (games.some(other)) t += '  * analysed with another profile (engine, network, depths or hash): not in the scores above\n';
    t += '\nAnomalies (latest 50)\n' + table(anomalies.map((a) => ({ at: iso(a.at), kind: a.kind, severity: a.severity, game: a.gameId || '' })), ['at', 'kind', 'severity', 'game']);
    // The store's report rows carry createdAt and status ('open' until resolved).
    t += '\nReports received\n' + table(reports.map((r) => ({
        id: r.id, at: iso(r.createdAt ?? r.at), category: r.category, weight: r.weight, game: r.gameId,
        outcome: r.outcome ?? r.resolution ?? (r.status && r.status !== 'open' ? r.status : 'open'), comment: String(r.comment || '').slice(0, 60),
    })), ['id', 'at', 'category', 'weight', 'game', 'outcome', 'comment']);
    t += '\nSanctions\n' + table(sanctions.map((x) => ({ id: x.id, kind: x.kind, source: x.source, until: iso(x.endsAt), reason: x.reason })), ['id', 'kind', 'source', 'until', 'reason']);
    return { data, text: t };
}

function integrityConfirm(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    const reason = textFlag(ctx, 'reason', { required: true });
    const hours = intFlag(ctx, 'hours', { min: 1, max: 87600, def: ctx.config?.banDurationHours ?? 24 });
    const noRefund = !!ctx.args.flags['no-refund'];
    const since = dateFlag(ctx, 'refund-since');
    if (noRefund && since !== null) throw new AdminError('--refund-since and --no-refund exclude each other');
    const now = ctx.now();
    const prev = readIntegrity(ctx.store, u.id);
    const ev = { ...prev.evidence };
    pushReview(ev, { action: 'confirm', by: ctx.moderator, at: now, reason, previousLevel: prev.level, score: prev.score });
    ev.review = { ...(ev.review || {}), confirmedAt: now, by: ctx.moderator };
    writeIntegrity(ctx.store, u.id, { level: 'confirmed', score: prev.score, evidence: ev, updatedAt: now, reviewedBy: ctx.moderator });
    const until = now + hours * HOUR_MS;
    // The reason tells the store whether the games recorded during the ban are refunded
    // (refunds.js banRefunds): not after --no-refund.
    const banReason = `${noRefund ? CheatBanReason.confirmedNoRefund : CheatBanReason.confirmed}${reason}`;
    const id = ctx.store.sanctions.create({ userId: u.id, kind: 'ban', reason: banReason, source: 'moderator', gameId: null, startsAt: now, endsAt: until, createdBy: ctx.moderator });
    const resolved = ctx.args.flags['keep-reports'] ? 0 : resolveOpenCheatingReports(ctx, u.id, 'actioned');
    const from = noRefund ? null : since ?? refundWindowStart(ctx.config, now);
    // The ban stands whatever happens to the refunds (one transaction of their own): a failure is
    // audited with the ban and reported, and `refunds apply` gives them later.
    let refunds = [], refundError = null;
    if (from !== null) {
        try {
            refunds = refundVictims(ctx.store, { cheaterId: u.id, since: from, now, sanctionId: id, source: 'moderator', by: ctx.moderator, log: ctx.log });
        } catch (e) {
            refundError = e.message || String(e);
        }
    }
    const victims = victimTotals(refunds);
    audit(ctx, 'integrity_confirm', u.id, { reason, previousLevel: prev.level, score: prev.score, sanctionId: id, until, reportsActioned: resolved,
        refundSince: from, refunds: refunds.length, refundedVictims: victims.length, refundedPoints: sumPoints(refunds), refundError });
    if (refundError) {
        throw new AdminError(`${u.username}: integrity confirmed and banned until ${iso(until)} (sanction #${id}), but the rating refunds failed `
            + `(${refundError}): give them with \`refunds apply ${u.username}${since !== null ? ` --since ${iso(since)}` : ''}\``);
    }
    return { data: { level: 'confirmed', sanctionId: id, until, reportsActioned: resolved, refundSince: from, refunds },
        text: `${u.username}: integrity confirmed (was ${prev.level}), banned until ${iso(until)} (sanction #${id}), ${resolved} open cheating report(s) marked actioned.\n`
            + refundText(ctx, from, refunds) };
}

function sumPoints(refunds) { return refunds.reduce((n, r) => n + r.points, 0); }

function refundText(ctx, from, refunds) {
    if (from === null) return 'No rating refunds (--no-refund, or RATING_REFUND_DAYS=0).\n';
    const victims = victimTotals(refunds);
    const name = (id) => safe(() => ctx.store.users.byId(id)?.username, null) ?? `#${id}`;
    let t = `Rating refunds of the games since ${iso(from)}: ${refunds.length} game(s), ${sumPoints(refunds)} point(s) to ${victims.length} player(s)`
        + (refunds.length ? ' (they are told at their next moment out of a game).\n' : '.\n');
    if (refunds.length) {
        t += table(refunds.map((r) => ({ game: r.gameId, victim: name(r.victimId), category: r.category, points: r.points, ended: iso(r.endedAt) })),
            ['game', 'victim', 'category', 'points', 'ended']);
    }
    return t;
}

function integrityClear(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    const reason = textFlag(ctx, 'reason');
    const now = ctx.now();
    const prev = readIntegrity(ctx.store, u.id);
    const ev = { ...prev.evidence };
    pushReview(ev, { action: 'clear', by: ctx.moderator, at: now, reason, previousLevel: prev.level, score: prev.score });
    // The automatic model only raises the level again on new evidence (scoring.js).
    ev.review = { clearedAt: now, clearedScore: prev.score, by: ctx.moderator };
    writeIntegrity(ctx.store, u.id, { level: 'none', score: prev.score, evidence: ev, updatedAt: now, reviewedBy: ctx.moderator });
    const dismissed = ctx.args.flags['dismiss-reports'] ? resolveOpenCheatingReports(ctx, u.id, 'dismissed') : 0;
    audit(ctx, 'integrity_clear', u.id, { reason, previousLevel: prev.level, score: prev.score, reportsDismissed: dismissed });
    return { data: { level: 'none', previous: prev.level, reportsDismissed: dismissed },
        text: `${u.username}: integrity cleared (was ${prev.level}).${dismissed ? ` ${dismissed} open cheating report(s) dismissed.` : ''} Bans are not lifted by this command (user unban).\n` };
}

// ---- reports, anomalies, stats -------------------------------------------------------------------------

function reportsList(ctx) {
    const limit = intFlag(ctx, 'limit', { min: 1, max: 10000, def: 100 });
    const now = ctx.now();
    const groups = new Map();
    for (const r of ctx.store.reports.listOpen(limit) || []) {
        const id = r.reportedId ?? r.reported_id;
        let g = groups.get(id);
        if (!g) { g = { reportedId: id, reports: [], weight: 0 }; groups.set(id, g); }
        g.reports.push(r);
        g.weight += Number(r.weight) || 0;
    }
    const rows = [];
    for (const g of groups.values()) {
        const u = safe(() => ctx.store.users.byId(g.reportedId), null);
        const integ = readIntegrity(ctx.store, g.reportedId);
        const w30 = recentReportWeight(reportsAgainst(ctx, g.reportedId), now, 30) || g.weight;
        rows.push({
            reportedId: g.reportedId, username: u?.username ?? `#${g.reportedId}`, level: integ.level, score: integ.score,
            priority: reviewPriority({ level: integ.level, score: integ.score, reportWeight: w30 }),
            open: g.reports.length, weight: Math.round(g.weight * 1000) / 1000,
            categories: [...new Set(g.reports.map((r) => r.category))].join(','),
            ids: g.reports.map((r) => r.id), latest: Math.max(...g.reports.map((r) => Number(r.createdAt ?? r.at) || 0)),
        });
    }
    rows.sort((a, b) => b.priority - a.priority || b.weight - a.weight);
    const text = table(rows.map((r) => ({ ...r, ids: r.ids.join(','), latest: iso(r.latest), score: r.score.toFixed(2) })),
        ['priority', 'username', 'level', 'score', 'open', 'weight', 'categories', 'latest', 'ids']);
    return { data: rows, text };
}

function reportsResolve(ctx) {
    const idText = ctx.args.positional[2];
    const outcome = ctx.args.positional[3];
    if (!/^\d+$/.test(String(idText || ''))) throw new AdminError('reports resolve <id> actioned|dismissed');
    if (outcome !== 'actioned' && outcome !== 'dismissed') throw new AdminError('the outcome is actioned or dismissed');
    const id = Number(idText);
    const r = ctx.store.reports.resolve(id, outcome, ctx.moderator, ctx.now());
    if (r === false || r === 0) throw new AdminError(`report #${id} not found or already resolved`);
    audit(ctx, 'report_resolve', null, { reportId: id, outcome });
    return { data: { id, outcome }, text: `Report #${id} ${outcome}.\n` };
}

function anomaliesCmd(ctx) {
    const u = requireUser(ctx, ctx.args.positional[1]);
    const limit = intFlag(ctx, 'limit', { min: 1, max: 10000, def: 50 });
    const rows = (ctx.store.anomalies.forUser(u.id, limit) || []).map((a) => ({ ...a, detail: parseMaybeJson(a.detail, a.detail) }));
    const text = table(rows.map((a) => ({ at: iso(a.at), kind: a.kind, severity: a.severity, game: a.gameId || '', detail: JSON.stringify(a.detail ?? '').slice(0, 80) })), ['at', 'kind', 'severity', 'game', 'detail']);
    return { data: rows, text };
}

function stats(ctx) {
    const counts = { suspected: 0, high_confidence: 0, confirmed: 0 };
    for (const r of safe(() => ctx.store.integrity.listFlagged('suspected', 100000), [])) if (r.level in counts) counts[r.level]++;
    const openReports = safe(() => ctx.store.reports.listOpen(100000), []).length;
    const data = { integrity: counts, openReports, store: null };
    let t = `Integrity: ${counts.suspected} suspected, ${counts.high_confidence} high confidence, ${counts.confirmed} confirmed\n`;
    t += `Open reports: ${openReports}\n`;
    return { data, text: t };
}

// ---- rating refunds ------------------------------------------------------------------------------------

function refundsApply(ctx) {
    const u = requireUser(ctx, ctx.args.positional[2]);
    if (readIntegrity(ctx.store, u.id).level !== 'confirmed') {
        throw new AdminError(`${u.username} is not a confirmed cheater (integrity confirm first)`);
    }
    const now = ctx.now();
    // The window counts back from the latest ban for cheating, not from a later ban for something
    // else (`user ban`).
    const bans = safe(() => ctx.store.sanctions.list(u.id), []).filter((x) => isCheatingBan(x) && (x.startsAt ?? 0) <= now)
        .sort((a, b) => (b.startsAt ?? 0) - (a.startsAt ?? 0));
    const ban = bans[0] || null;
    const since = dateFlag(ctx, 'since') ?? refundWindowStart(ctx.config, ban ? ban.startsAt : now);
    if (since === null) throw new AdminError('RATING_REFUND_DAYS is 0: give the start of the refunds with --since DATE');
    const refunds = refundVictims(ctx.store, { cheaterId: u.id, since, now, sanctionId: ban?.id ?? null, source: 'moderator', by: ctx.moderator, log: ctx.log });
    audit(ctx, 'refunds_apply', u.id, { since, sanctionId: ban?.id ?? null, refunds: refunds.length, refundedVictims: victimTotals(refunds).length,
        refundedPoints: sumPoints(refunds) });
    return { data: { since, sanctionId: ban?.id ?? null, refunds }, text: `${u.username}: ` + refundText(ctx, since, refunds) };
}

function refundsList(ctx) {
    const limit = intFlag(ctx, 'limit', { min: 1, max: 10000, def: 100 });
    const cheater = ctx.args.positional[2] ? requireUser(ctx, ctx.args.positional[2]) : null;
    const victimName = ctx.args.flags.victim;
    if (victimName === true) throw new AdminError('--victim NAME');
    const victim = victimName === undefined ? null : requireUser(ctx, String(victimName));
    if (cheater && victim) throw new AdminError('give a cheater or --victim, not both');
    const rows = ctx.store.refunds.list({ cheaterId: cheater?.id ?? null, victimId: victim?.id ?? null, limit });
    const text = table(rows.map((r) => ({
        id: r.id, at: iso(r.createdAt), game: r.gameId, cheater: r.cheaterName, victim: r.victimName, category: r.category, points: r.points,
        source: r.source, by: r.createdBy || (r.sanctionId ? `ban #${r.sanctionId}` : ''), notified: r.notifiedAt ? iso(r.notifiedAt) : 'not yet',
    })), ['id', 'at', 'game', 'cheater', 'victim', 'category', 'points', 'source', 'by', 'notified']);
    return { data: rows, text };
}

// ---- bench accounts ------------------------------------------------------------------------------------

/** Default session-token hash (hex SHA-256); bin/admin.js passes the auth module's when it has one. */
export function defaultHashToken(token) {
    return crypto.createHash('sha256').update(token).digest('hex');
}

function benchAccounts(ctx) {
    if (!ctx.args.flags['i-know-this-is-a-test-server']) {
        throw new AdminError('bench-accounts creates verified accounts with live sessions; refused without --i-know-this-is-a-test-server');
    }
    const count = intFlag(ctx, 'count', { min: 1, max: 100000 });
    const prefix = ctx.args.flags.prefix === undefined ? 'bench' : String(ctx.args.flags.prefix);
    if (!/^[A-Za-z][A-Za-z0-9_]{0,15}$/.test(prefix)) throw new AdminError('--prefix: letters, digits and _ (starting with a letter)');
    const out = textFlag(ctx, 'out', { required: true, max: 4096 });
    const format = ctx.args.flags.format === undefined ? 'tokens' : String(ctx.args.flags.format);
    if (format !== 'tokens' && format !== 'tsv') throw new AdminError('--format tokens|tsv');
    const width = Math.max(4, String(count).length);
    const maxLen = ctx.config?.usernameMax ?? 20;
    if (prefix.length + width > maxLen) throw new AdminError(`usernames would exceed ${maxLen} characters: shorten --prefix`);
    const hash = ctx.hashToken || defaultHashToken;
    const now = ctx.now();
    const maxDays = ctx.config?.sessionMaxDays ?? 90, idleDays = ctx.config?.sessionIdleDays ?? 30;
    const lines = [];
    let created = 0, reused = 0;
    for (let i = 1; i <= count; i++) {
        const username = prefix + String(i).padStart(width, '0');
        const email = `${username.toLowerCase()}@bench.invalid`;
        let u = ctx.store.users.byUsername(username);
        if (u && String(u.email || '').toLowerCase() !== email) throw new AdminError(`"${username}" exists and is not a bench account`);
        if (!u) {
            // No usable password: bench accounts only log in with the tokens written here.
            const id = ctx.store.users.create({ username, email, passwordHash: '!bench-account-no-password', emailVerified: true });
            u = { id };
            created++;
        } else {
            if (!u.emailVerified) ctx.store.users.update(u.id, { emailVerified: true });
            reused++;
        }
        const token = 'sct_' + crypto.randomBytes(32).toString('base64url');
        ctx.store.sessions.create({ userId: u.id, tokenHash: hash(token), createdAt: now, expiresAt: now + maxDays * DAY_MS,
            idleExpiresAt: now + idleDays * DAY_MS, clientLabel: 'bench', ip: null });
        lines.push(format === 'tsv' ? `${username}\t${token}` : token);
    }
    fs.writeFileSync(out, lines.join('\n') + '\n', { mode: 0o600 });
    audit(ctx, 'bench_accounts', null, { count, prefix, created, reused });
    return { data: { count, created, reused, out }, text: `${count} bench accounts ready (${created} created, ${reused} reused); tokens written to ${out} (mode 600).\n` };
}

// ---- backup --------------------------------------------------------------------------------------------

// VACUUM INTO writes a consistent snapshot in one pass. The sqlite3 shell's .backup copies 100
// pages at a time and starts over whenever another connection writes, so on a busy server it may
// never finish. The copy holds e-mail addresses and recent IPs: it is created with mode 600, and
// should be encrypted before it leaves the host. The source must be the server's database: a
// missing file (opening it would create an empty one) or a database without the schema (a wrong
// DB_PATH or DATA_DIR) is refused, so that a scheduled backup never silently copies nothing.
function backupCmd(ctx) {
    const target = ctx.args.positional[1];
    if (!target) throw new AdminError('backup <file>: the new file to write');
    const src = ctx.config?.dbPath;
    if (!src || src === ':memory:' || path.basename(src) === ':memory:') throw new AdminError('no database file to back up (DB_PATH)');
    const notOurs = (why) => new AdminError(`no Scacelith database at ${src} (${why}); check DB_PATH and DATA_DIR`);
    if (!fs.existsSync(src)) throw notOurs('no such file');
    const out = path.resolve(target);
    const t0 = performance.now();
    let db;
    try {
        db = new DatabaseSync(src);
        db.exec('PRAGMA busy_timeout = 5000');
        if (!db.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'").get()
            || !db.prepare('SELECT 1 FROM schema_migrations LIMIT 1').get()) throw notOurs('no schema_migrations');
    } catch (e) {
        db?.close();
        throw e instanceof AdminError ? e : notOurs(e.message);
    }
    try {
        try {
            fs.writeFileSync(out, '', { flag: 'wx', mode: 0o600 });   // VACUUM INTO accepts an empty file
        } catch (e) {
            throw new AdminError(e.code === 'EEXIST' ? `${target} exists: choose a new file` : `cannot create ${target}: ${e.message}`);
        }
        try {
            db.prepare('VACUUM INTO ?').run(out);
        } catch (e) {
            try { fs.unlinkSync(out); } catch { /* already gone */ }
            throw new AdminError(`backup failed: ${e.message}`);
        }
    } finally {
        db.close();
    }
    const ms = Math.round(performance.now() - t0);
    let verified = false;
    if (ctx.args.flags.verify) {
        const b = new DatabaseSync(out, { readOnly: true });
        let check;
        try { check = b.prepare('PRAGMA quick_check').all().map((r) => Object.values(r)[0]).join('; '); } finally { b.close(); }
        if (check !== 'ok') throw new AdminError(`backup written to ${target} but quick_check failed: ${check}`);
        verified = true;
    }
    const bytes = fs.statSync(out).size;
    return { data: { file: out, bytes, ms, verified },
        text: `Backup written to ${out} (${(bytes / 1048576).toFixed(1)} MB in ${ms} ms${verified ? ', quick_check ok' : ''}).\n` };
}

function safe(fn, fallback) {
    try { const v = fn(); return v === undefined ? fallback : v; } catch { return fallback; }
}

/** Command table: "<group> <name>" -> handler(ctx) -> { data, text }. */
export const COMMANDS = Object.freeze({
    'user show': userShow,
    'user ban': userBan,
    'user unban': userUnban,
    'user reset-mfa': userResetMfa,
    'user verify-email': userVerifyEmail,
    'user revoke-sessions': userRevokeSessions,
    'integrity list': integrityList,
    'integrity show': integrityShow,
    'integrity confirm': integrityConfirm,
    'integrity clear': integrityClear,
    'reports list': reportsList,
    'reports resolve': reportsResolve,
    'refunds apply': refundsApply,
    'refunds list': refundsList,
    'anomalies': anomaliesCmd,
    'stats': stats,
    'bench-accounts': benchAccounts,
    'backup': backupCmd,
});

/**
 * Runs one admin command.
 * @param {string[]} argv   arguments after the program name
 * @param {{ store: object, config?: object, out?: {write: Function}, err?: {write: Function}, now?: () => number, moderator?: string, log?: object, hashToken?: Function }} env
 * @returns {Promise<number>} exit code (0 ok, 1 error, 2 usage)
 */
export async function runAdmin(argv, { store, config = null, out = process.stdout, err = process.stderr, now = Date.now, moderator = 'admin', log = null, hashToken = null } = {}) {
    const args = parseArgs(argv);
    const [a, b] = args.positional;
    // Own keys only: an inherited name (constructor, toString, __proto__) is not a command.
    const key = Object.hasOwn(COMMANDS, `${a} ${b}`) ? `${a} ${b}` : Object.hasOwn(COMMANDS, a) ? a : null;
    const handler = key ? COMMANDS[key] : null;
    if (!handler || args.flags.help) {
        (handler ? out : err).write(USAGE);
        return handler ? 0 : 2;
    }
    const by = typeof args.flags.by === 'string' && args.flags.by.trim() ? args.flags.by.trim().slice(0, 64) : moderator;
    const ctx = { store, config, now, moderator: by, log, args, hashToken };
    try {
        const res = await handler(ctx);
        out.write(args.flags.json ? JSON.stringify(res.data, null, 2) + '\n' : res.text);
        return 0;
    } catch (e) {
        if (e instanceof AdminError) { err.write(`error: ${e.message}\n`); return 1; }
        throw e;
    }
}
