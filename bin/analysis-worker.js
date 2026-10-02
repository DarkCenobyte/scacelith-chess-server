#!/usr/bin/env node
// Engine-analysis process of the anti-cheat (started by the primary through
// startAnalysisProcess, or by hand for maintenance). Started by the primary, it runs the
// configuration the primary loaded at its start, as the shards do (config.js primaryConfig: a
// restart after an edit of .env or of a secret file keeps it); started by hand, it reads its own
// like the server (environment + .env). It opens the database, lowers its own CPU priority and
// analyses the queued games until it receives SIGTERM / SIGINT or a { type: 'shutdown' } IPC
// message. Started by the primary, it answers the primary's 'metrics.snapshot' requests (its
// metrics in /metrics).

import os from 'node:os';
import { Ipc } from '../src/cluster/ipc.js';
import { loadConfig, primaryConfig } from '../src/config.js';
import { configureLogging, logger } from '../src/log.js';
import { metrics } from '../src/metrics.js';
import { createAnalysisWorker } from '../src/anticheat/analysis/worker.js';

async function main() {
    const log = logger.child('analysis');
    const ipc = process.send ? new Ipc(process, { name: 'primary', log }) : null;
    const config = ipc ? await primaryConfig(ipc) : loadConfig();
    configureLogging({ level: config.logLevel, format: config.logFormat, ipMode: config.logIp, secret: config.serverSecret, base: { proc: 'analysis' } });
    if (!config.analysisEnginePath || !config.analysisWorkers) {
        log.info('engine analysis disabled (ANALYSIS_ENGINE_PATH empty or ANALYSIS_WORKERS=0)');
        ipc?.close('disabled');
        return 0;
    }
    try { os.setPriority(0, os.constants.priority.PRIORITY_LOW); } catch { /* not permitted */ }

    const { openStore } = await import('../src/store/index.js');
    let applyGame;
    try { ({ applyGame } = await import('../src/match/elo.js')); } catch { applyGame = undefined; }
    const store = openStore(config, { applyGame });
    const worker = createAnalysisWorker({ config, store, log });
    ipc?.on('metrics.snapshot', () => metrics.snapshot());

    let stopping = null;
    const stop = (why) => {
        if (!stopping) {
            log.info('analysis process stopping', { why });
            stopping = worker.stop();
        }
        return stopping;
    };
    process.on('SIGTERM', () => stop('SIGTERM'));
    process.on('SIGINT', () => stop('SIGINT'));
    process.on('message', (m) => { if (m && m.type === 'shutdown') stop('shutdown message'); });
    process.on('disconnect', () => stop('primary gone'));

    await worker.run();
    await stop('done');
    try { store.close(); } catch { /* already closed */ }
    ipc?.close('stopped');
    if (process.connected) process.disconnect();   // the IPC channel would keep the process alive
    return 0;
}

main().then((code) => { process.exitCode = code; }, (e) => {
    logger.child('analysis').error('analysis process failed', { err: e });
    process.exitCode = 1;
    if (process.connected) process.disconnect();
});
