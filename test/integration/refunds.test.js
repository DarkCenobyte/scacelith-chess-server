// Rating refunds end to end (docs/ANTICHEAT.md, rating refunds), on the real server with two
// shards: a player who beat three rated opponents is banned automatically for an illegal move;
// each opponent gets the points they lost back, and Notice{RatingRestored} out of a game only: at
// once for the one who is connected and idle, after the game in progress for the one who is
// playing, right after Welcome for the one who was offline. Needs the openssl command line
// (skipped without it).
import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { startServer, haveOpenssl } from './helpers/harness.js';
import { connect, player, challengeGame, Table, closeAll } from './helpers/players.js';
import { enums, CloseCode } from '../../src/protocol/index.js';

const { GameStatus: GS, EndReason: ER, NoticeCode: N } = enums;
const skip = !haveOpenssl() && 'openssl not available';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

let srv;
before(async () => {
    if (skip) return;
    srv = await startServer({ workers: 2, env: { FIRST_MOVE_TIMEOUT_MS: '20000', DB_COMMIT_MS: '20', LOG_LEVEL: 'warn' } });
});
after(async () => { if (srv) await srv.stop(); });

function db() {
    const raw = new DatabaseSync(path.join(srv.dir, 'scacelith.db'));
    raw.exec('PRAGMA busy_timeout = 5000');
    return raw;
}

// Established players (40 games, K 20) in 3+2, so that a lost game costs K-formula points.
function seedRated(...players) {
    const raw = db();
    const put = raw.prepare(`INSERT INTO ratings (user_id, category, rating, games, wins, losses, peak, rated, updated_at)
        VALUES (?, '3+2', 1500, 40, 20, 20, 1500, 1, 0)`);
    for (const p of players) put.run(p.client.welcome.userId);
    raw.close();
}

function ratingOf(userId) {
    const raw = db();
    const r = raw.prepare(`SELECT rating FROM ratings WHERE user_id = ? AND category = '3+2'`).get(userId);
    raw.close();
    return r.rating;
}

// A rated 3+2 game that `winner` (White) wins by resignation after two moves; resolves with the
// points the loser lost once the game is rated.
async function loseTo(winner, loser) {
    const g = await challengeGame(winner, loser, { baseSec: 180, incSec: 2, rated: true });
    const t = new Table(g);
    await t.playAll(['e2e4', 'e7e5']);
    const m = loser.client.mark();
    loser.client.resign(g.id);
    const ru = await loser.client.waitFor('RatingUpdate', (x) => x.game === g.id, 10000, { since: m });
    return ru.black.before - ru.black.after;
}

const restored = (p, since, timeoutMs) => p.client.waitFor('Notice', (m) => m.code === N.RatingRestored, timeoutMs, { since });

test('a banned cheater\'s victims get their points back and are told out of a game only', { skip }, async () => {
    const cheat = await player(srv, 'cheat'), idle = await player(srv, 'idle'), busy = await player(srv, 'busy');
    const away = await player(srv, 'away'), other = await player(srv, 'other'), target = await player(srv, 'target');
    try {
        seedRated(cheat, idle, busy, away);
        const id = Object.fromEntries(Object.entries({ cheat, idle, busy, away }).map(([k, p]) => [k, p.client.welcome.userId]));
        const lost = {};
        for (const [name, v] of Object.entries({ idle, busy, away })) {
            lost[name] = await loseTo(cheat, v);
            assert.ok(lost[name] >= 9 && lost[name] <= 10, `K 20, the cheater 0 to 20 points higher: ${lost[name]}`);
        }
        const cheatRating = ratingOf(id.cheat);
        // `away` goes offline; `busy` starts a game with `other`.
        await away.client.close();
        const g = await challengeGame(busy, other);
        const t = new Table(g);
        await t.play('d2d4');
        const mIdle = idle.client.mark(), mBusy = busy.client.mark();

        // The cheat: an illegal move in a synchronised position bans the cheater at once.
        const cg = await challengeGame(cheat, target);
        const ct = new Table(cg);
        await ct.playAll(['e2e4', 'e7e5']);
        const closed = cheat.client.waitFor('close', null, 5000);
        ct.send('e1e3');
        assert.equal((await closed).code, CloseCode.CheatDetected);

        // Connected and idle: told at once, with the points given back.
        assert.equal((await restored(idle, mIdle, 5000)).arg, lost.idle);
        assert.equal(ratingOf(id.idle), 1500);
        // Playing: the refund is applied, the notice waits for the end of the game.
        assert.equal(ratingOf(id.busy), 1500);
        await sleep(1000);
        await assert.rejects(restored(busy, mBusy, 10), 'no notice during a game');
        const mEnd = busy.client.mark();
        busy.client.resign(g.id);
        const end = await busy.client.waitFor('GameEnd', (x) => x.game === g.id, 5000, { since: mEnd });
        assert.deepEqual([end.status, end.reason], [GS.BlackWins, ER.Resignation]);
        assert.equal((await restored(busy, mEnd, 5000)).arg, lost.busy);
        // Offline: told right after Welcome at the next connection.
        assert.equal(ratingOf(id.away), 1500);
        away.client = await connect(srv, away.token);
        assert.equal((await restored(away, 0, 5000)).arg, lost.away);

        // Each refund is marked notified (once the shard has answered that it wrote the notice,
        // which may come a moment after the client read it); the cheater's own rating is left alone.
        const refunds = () => {
            const raw = db();
            const rows = raw.prepare('SELECT victim_id, points, source, notified_at FROM rating_refunds ORDER BY victim_id').all();
            raw.close();
            return rows;
        };
        for (let i = 0; i < 40 && refunds().some((r) => !r.notified_at); i++) await sleep(50);
        const rows = refunds();
        assert.deepEqual(rows.map((r) => [r.victim_id, r.points, r.source]),
            [[id.idle, lost.idle, 'auto'], [id.busy, lost.busy, 'auto'], [id.away, lost.away, 'auto']].sort((a, b) => a[0] - b[0]));
        assert.ok(rows.every((r) => r.notified_at > 0), JSON.stringify(rows));
        assert.equal(ratingOf(id.cheat), cheatRating);
    } finally { await closeAll(cheat, idle, busy, away, other, target); }
});
