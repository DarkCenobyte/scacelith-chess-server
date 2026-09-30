// A minimal UCI engine for the driver's tests, run as `node fake-uci-engine.js [INFO_STRING...]`:
// it answers uci and isready, answers every go with a one-line search of e2e4, and prints the
// given info strings before it (what Stockfish reports when a search starts: the network, and
// with Stockfish 19 and later where it lives).

import readline from 'node:readline';

const strings = process.argv.slice(2);
readline.createInterface({ input: process.stdin }).on('line', (line) => {
    const cmd = line.trim().split(/\s+/)[0];
    if (cmd === 'uci') process.stdout.write('id name Fake Engine 1\nuciok\n');
    else if (cmd === 'isready') process.stdout.write('readyok\n');
    else if (cmd === 'go') {
        process.stdout.write(strings.map((s) => `info string ${s}\n`).join('')
            + 'info depth 1 seldepth 1 multipv 1 score cp 20 nodes 20 pv e2e4\nbestmove e2e4\n');
    } else if (cmd === 'quit') process.exit(0);
});
