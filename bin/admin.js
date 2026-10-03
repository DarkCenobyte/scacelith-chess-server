#!/usr/bin/env node
// Administration CLI of a Scacelith server. Runs on the server host, directly against the
// database (no network): the same configuration as the server (environment + .env), the same
// SQLite file (WAL mode lets it run while the server is up). `scacelith-admin --help` lists the
// commands; every moderator action is audited (security event + reviewed_by / created_by).

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { loadConfig, ConfigError } from '../src/config.js';
import { configureLogging, logger } from '../src/log.js';
import { runAdmin, USAGE } from '../src/anticheat/admin.js';
import { sha256Hex } from '../src/security/keys.js';    // auth/sessions.js's session-token hash

function moderatorName() {
    if (process.env.SCACELITH_MODERATOR) return process.env.SCACELITH_MODERATOR;
    if (process.env.SUDO_USER) return process.env.SUDO_USER;
    try { return os.userInfo().username; } catch { return 'admin'; }
}

async function main(argv) {
    if (!argv.length || argv[0] === '--help' || argv[0] === 'help') {
        process.stdout.write(USAGE);
        return argv.length ? 0 : 2;
    }
    let config;
    try { config = loadConfig(); } catch (e) {
        if (e instanceof ConfigError) { process.stderr.write(e.message + '\n'); return 1; }
        throw e;
    }
    configureLogging({ level: 'warn', format: 'pretty', ipMode: config.logIp, secret: config.serverSecret, out: process.stderr, base: { proc: 'admin' } });
    // Every command works on the server's existing database: opening a missing file would create
    // an empty one (a wrong DB_PATH, or a relative DATA_DIR run from another directory).
    if (path.basename(config.dbPath) !== ':memory:' && !fs.existsSync(config.dbPath)) {
        process.stderr.write(`error: no Scacelith database at ${config.dbPath}; check DB_PATH and DATA_DIR (a relative DATA_DIR is resolved from the current directory)\n`);
        return 1;
    }
    const { openStore } = await import('../src/store/index.js');
    let applyGame;
    try { ({ applyGame } = await import('../src/match/elo.js')); } catch { applyGame = undefined; }
    const store = openStore(config, { applyGame });
    try {
        return await runAdmin(argv, { store, config, moderator: moderatorName(), log: logger.child('admin'), hashToken: sha256Hex });
    } finally {
        try { store.close(); } catch { /* ignore */ }
    }
}

main(process.argv.slice(2)).then((code) => { process.exitCode = code; }, (e) => {
    process.stderr.write(`admin: ${e.stack || e.message}\n`);
    process.exitCode = 1;
});
