#!/usr/bin/env node
// Scacelith dedicated server.
//
//   scacelith-server [start]       start the server (primary + WORKERS shard processes)
//   scacelith-server migrate       apply the database migrations and exit
//   scacelith-server check-config  validate the configuration and print it (secrets hidden)
//   scacelith-server gen-secret    print a new random SERVER_SECRET value
//
// Configuration: environment variables and ./.env (or SCACELITH_ENV_FILE); see .env.example and
// docs/CONFIG.md.

import crypto from 'node:crypto';
import { ConfigError, describe, loadConfig } from '../src/config.js';

const USAGE = `Usage: scacelith-server [command]

Commands:
  start          Start the server (default).
  migrate        Apply the database migrations, then exit.
  check-config   Validate the configuration and print it without secrets.
  gen-secret     Print a new random value for SERVER_SECRET.
  help           Show this help.
`;

function configOrExit() {
    try {
        return loadConfig();
    } catch (e) {
        if (e instanceof ConfigError) {
            process.stderr.write(e.message + '\n');
            process.exit(1);
        }
        throw e;
    }
}

async function run(cmd) {
    switch (cmd) {
        case 'start': {
            configOrExit();
            const { main } = await import('../src/cluster/primary-main.js');
            await main();
            return;
        }
        case 'migrate': {
            const config = configOrExit();
            const { openStore, migrate } = await import('../src/store/index.js');
            const { applyGame } = await import('../src/match/elo.js');
            const { ensureServerId } = await import('../src/cluster/primary-main.js');
            const store = openStore(config, { applyGame });
            try {
                const applied = await migrate(store);
                const serverId = ensureServerId(store);
                process.stdout.write(JSON.stringify({ ok: true, applied: applied ?? null, serverId }, null, 2) + '\n');
            } finally {
                store.close();
            }
            return;
        }
        case 'check-config': {
            const config = configOrExit();
            process.stdout.write(JSON.stringify(describe(config), null, 2) + '\n');
            return;
        }
        case 'gen-secret':
            process.stdout.write(crypto.randomBytes(48).toString('base64') + '\n');
            return;
        case 'help': case '-h': case '--help':
            process.stdout.write(USAGE);
            return;
        default:
            process.stderr.write(`Unknown command "${cmd}".\n\n${USAGE}`);
            process.exit(2);
    }
}

run(process.argv[2] || 'start').catch((e) => {
    process.stderr.write(`scacelith-server: ${e && e.stack ? e.stack : e}\n`);
    process.exit(1);
});
