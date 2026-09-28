// Players for the integration tests: an account on the real server (HTTPS API) and a connected
// realtime client (SDK), plus a local copy of the rules to compute each move's posHash.
import assert from 'node:assert/strict';
import { ApiClient, ScacelithClient } from '../../../src/client/index.js';
import { ChessGame } from '../../../src/chess/index.js';
import { uciToMove } from '../../../src/protocol/index.js';

export const PASSWORD = 'correct horse battery staple 9';

/** Registers `name` (unless `register: false`), logs in and returns { api, token, name }. */
export async function account(srv, name, { register = true } = {}) {
    const api = new ApiClient({ host: '127.0.0.1', port: srv.apiPort, ca: srv.ca, servername: 'localhost' });
    if (register) {
        const r = await api.register({ username: name, email: `${name}@example.org`, password: PASSWORD });
        assert.ok(r.status === 201 || r.status === 202, `register ${name}: ${r.status} ${JSON.stringify(r.body)}`);
    }
    const l = await api.login(name, PASSWORD);
    assert.equal(l.status, 200, `login ${name}: ${JSON.stringify(l.body)}`);
    return { api, token: api.token, name };
}

/** Opens a realtime connection with `token`; resolves with the client (Welcome received). */
export function connect(srv, token, opts = {}) {
    return ScacelithClient.connect({ host: '127.0.0.1', wsPort: srv.wsPort, token, ca: srv.ca, servername: 'localhost', ...opts });
}

/** A registered, logged-in and connected player. */
export async function player(srv, name) {
    const acc = await account(srv, name);
    const client = await connect(srv, acc.token);
    return { ...acc, client };
}

/**
 * Starts a game between a and b with a direct challenge (a challenges b, b accepts) and returns
 * { id, white, black } where white/black are the players by colour. Colour: a gets White.
 */
export async function challengeGame(a, b, { baseSec = 180, incSec = 2, rated = false } = {}) {
    const ma = a.client.mark(), mb = b.client.mark();
    a.client.challenge(b.name, baseSec, incSec, rated, 1 /* ColorPref.White */);
    const rec = await b.client.waitFor('ChallengeReceived', null, 5000, { since: mb });
    b.client.acceptChallenge(rec.id);
    const sa = await a.client.waitFor('GameSnapshot', null, 5000, { since: ma });
    const sb = await b.client.waitFor('GameSnapshot', (m) => m.game === sa.game, 5000, { since: mb });
    assert.equal(sa.you, 0);
    assert.equal(sb.you, 1);
    return { id: sa.game, white: a, black: b };
}

/** Both players queue in `category` (rated) and get paired with each other. */
export async function queueGame(a, b, category = '3+2') {
    const ma = a.client.mark(), mb = b.client.mark();
    a.client.joinQueue(category, true);
    b.client.joinQueue(category, true);
    const sa = await a.client.waitFor('GameSnapshot', null, 10000, { since: ma });
    const sb = await b.client.waitFor('GameSnapshot', (m) => m.game === sa.game, 10000, { since: mb });
    return { id: sa.game, white: sa.you === 0 ? a : b, black: sa.you === 0 ? b : a, snapshots: [sa, sb] };
}

/** Local mirror of a game's position (for posHash) and helpers to play it through the server. */
export class Table {
    constructor(game) {
        this.game = game;          // { id, white, black }
        this.rules = new ChessGame();
    }
    get ply() { return this.rules.moves.length; }
    get hash() { return this.rules.position.digest(); }
    side() { return this.ply % 2 === 0 ? this.game.white : this.game.black; }
    other() { return this.ply % 2 === 0 ? this.game.black : this.game.white; }

    /** Sends the move for the side to move; returns the seq. Does not update the mirror. */
    send(uci, { thinkMs = 500, drawOffer = false, by = this.side(), ply = this.ply, hash = this.hash } = {}) {
        return by.client.move(this.game.id, ply, uciToMove(uci), hash, thinkMs, drawOffer);
    }

    /** Plays one move and waits until both players received its MoveMade; returns the MoveMade. */
    async play(uci, opts = {}) {
        const mover = this.side(), other = this.other();
        const ply = this.ply;
        const mm = mover.client.mark(), mo = other.client.mark();
        this.send(uci, opts);
        const made = await mover.client.waitFor('MoveMade', (m) => m.game === this.game.id && m.ply === ply, 5000, { since: mm });
        await other.client.waitFor('MoveMade', (m) => m.game === this.game.id && m.ply === ply, 5000, { since: mo });
        const r = this.rules.play(uciToMove(uci));
        assert.ok(r.ok, `local rules refused ${uci}`);
        return made;
    }

    async playAll(list) {
        let last = null;
        for (const m of list) last = await this.play(m);
        return last;
    }
}

export const closeAll = (...players) => Promise.all(players.map((p) => p && p.client && p.client.state !== 'closed'
    ? p.client.close().catch(() => {}) : null).concat(players.map((p) => p && p.api && p.api.close())));
