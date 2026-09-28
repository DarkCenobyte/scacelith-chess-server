#!/usr/bin/env node
// Live interoperability check: the game's C++ OnlineClient against this server, over real TLS.
//
//   node tools/live-cpp-check.js [path/to/scacelith_tests]      (default ../build/scacelith_tests)
//   node tools/live-cpp-check.js wine path/to/scacelith_tests.exe   (Windows build: WinHTTP path)
//
// Starts a server (2 shards, self-signed certificate, HTTPS API and WSS on one port as in
// production, proof of work on registration), connects a Node bot that queues in rated 3+2 and
// plays random legal moves, then runs the C++ test `net_live_server_game`, which registers
// (solving the proof of work), logs in, queues, plays 12 plies against the bot and resigns. The
// C++ client trusts the server by pinning the SHA-256 of its certificate.
// Exit code: the C++ test's.
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { startServer } from '../test/integration/helpers/harness.js';
import { account, connect, closeAll } from '../test/integration/helpers/players.js';
import { ChessGame } from '../src/chess/index.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const command = process.argv.length > 2 ? process.argv.slice(2) : [path.join(HERE, '../../build/scacelith_tests')];
// Host name the C++ client connects to: the test certificate names both localhost and 127.0.0.1;
// Wine's WinHTTP only matches DNS names, so localhost is the default.
const HOST = process.env.LIVE_HOST || 'localhost';
const CPP_USER = 'cppplayer';
const CPP_PASSWORD = 'live check password 1234';

const srv = await startServer({ workers: 2, sharedPort: true, env: { POW_REGISTER_BITS: '12' } });
const pin = new crypto.X509Certificate(srv.ca).fingerprint256.replace(/:/g, '');
console.log(`server on ${HOST}:${srv.apiPort} (API + WSS), certificate SHA-256 ${pin}`);

let bot = null, code = 1;
try {
    const acc = await account(srv, 'nodebot');
    bot = { ...acc, client: await connect(srv, acc.token) };
    const rules = new ChessGame();
    let gameId = null, me = null, sent = -1, plies = 0;
    const playIfMyTurn = () => {
        if (gameId === null || rules.isOver || rules.moves.length % 2 !== me || sent >= rules.moves.length) return;
        const legal = rules.position.legalMoves();
        if (!legal.length) return;
        const mv = legal[Math.floor(Math.random() * legal.length)];
        sent = rules.moves.length;
        bot.client.move(gameId, sent, mv, rules.position.digest(), 200 + Math.floor(Math.random() * 300), false);
    };
    bot.client.on('GameSnapshot', (m) => {
        gameId = m.game; me = m.you;
        console.log(`bot: game ${gameId}, playing ${me === 0 ? 'White' : 'Black'}`);
        setTimeout(playIfMyTurn, 50);
    });
    bot.client.on('MoveMade', (m) => {
        if (m.game !== gameId) return;
        rules.play(m.move); plies++;
        setTimeout(playIfMyTurn, 20);
    });
    bot.client.on('GameEnd', (m) => { if (m.game === gameId) console.log(`bot: game over, status ${m.status} reason ${m.reason}`); });
    bot.client.on('RatingUpdate', (m) => console.log(`bot: rating update ${JSON.stringify(m)}`));
    bot.client.joinQueue('3+2', true);

    code = await new Promise((resolve) => {
        const child = spawn(command[0], [...command.slice(1), 'net_live'], {
            env: { ...process.env, SCACELITH_NET_LIVE: `${HOST}:${srv.apiPort}:${pin}:${CPP_USER}:${CPP_PASSWORD}` },
            stdio: ['ignore', 'inherit', 'inherit'],
        });
        child.on('exit', (c, sig) => resolve(c ?? (sig ? 128 : 1)));
        child.on('error', (e) => { console.error(`cannot run ${command.join(' ')}: ${e.message}`); resolve(127); });
    });
    console.log(`bot saw ${plies} plies; C++ test exit code ${code}`);
} catch (e) {
    console.error(e);
} finally {
    await closeAll(bot);
    await srv.stop();
}
process.exit(code);
