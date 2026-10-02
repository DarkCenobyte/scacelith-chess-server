// Worker thread of the GIF pool (src/gif/pool.js): renders one job at a time with renderGame and
// sends the GIF back as a transferred ArrayBuffer (no copy). The piece sprites, fonts, palette and
// square pictures stay cached in the thread between jobs.
//
// Messages in: { id, job }. Out: { id, ok: true, gif: ArrayBuffer, ms } or
// { id, ok: false, message, name }.

import { parentPort, isMainThread } from 'node:worker_threads';
import { renderGame } from './render.js';

if (!isMainThread && parentPort) {
    parentPort.on('message', (msg) => {
        const id = msg && msg.id;
        const t0 = performance.now();
        let gif;
        try {
            gif = renderGame(msg.job);
        } catch (e) {
            parentPort.postMessage({ id, ok: false, name: e && e.name ? String(e.name) : 'Error', message: String(e && e.message ? e.message : e) });
            return;
        }
        // renderGame returns a Buffer over an ArrayBuffer of its own: transfer it.
        const ab = gif.byteOffset === 0 && gif.byteLength === gif.buffer.byteLength
            ? gif.buffer
            : gif.buffer.slice(gif.byteOffset, gif.byteOffset + gif.byteLength);
        parentPort.postMessage({ id, ok: true, gif: ab, ms: performance.now() - t0 }, [ab]);
    });
}
