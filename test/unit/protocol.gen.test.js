// The generator: committed generated files are up to date, and schemas that cannot be encoded
// faithfully are refused with a clear message.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import * as schema from '../../src/protocol/schema.js';
import { CONFIG_KEYS } from '../../src/config.js';
import { generateCodec, generateDocs, buildModel, CODEC_PATH, DOCS_PATH } from '../../tools/gen-protocol.js';
import { renderVectors, VECTORS_PATH } from '../../tools/gen-protocol-vectors.js';

test('src/protocol/codec.gen.js is what the schema generates (run npm run gen:protocol)', () => {
    assert.equal(fs.readFileSync(CODEC_PATH, 'utf8'), generateCodec());
});

test('docs/PROTOCOL.md is what the schema generates', () => {
    assert.equal(fs.readFileSync(DOCS_PATH, 'utf8'), generateDocs());
});

test('test/fixtures/protocol-vectors.json is what the codec generates', () => {
    assert.equal(fs.readFileSync(VECTORS_PATH, 'utf8'), renderVectors());
});

test('docs/PROTOCOL.md tells clients about the clock hold of a game restored after a restart', () => {
    // The server holds the clock of the side to move of a restored game (RECOVERY_CLOCK_HOLD_MS),
    // shows running = None meanwhile, even after ply 1 (room.js snapshot), and sends the opponent a
    // GameSnapshot of its own accord when the held clock starts (host.js, Outcome.clockStarted).
    // Third-party clients follow the protocol reference, so it must say so.
    const docs = generateDocs();
    const flat = (s) => s.replace(/\s+/g, ' ');
    const section = (title) => {
        const i = docs.indexOf(`\n## ${title}\n`);
        assert.ok(i >= 0, `section ${title}`);
        const j = docs.indexOf('\n## ', i + 1);
        return flat(docs.slice(i, j < 0 ? undefined : j));
    };
    const lifecycle = section('Connection lifecycle');
    const step6 = lifecycle.slice(lifecycle.indexOf(' 6. **Reconnection.**'), lifecycle.indexOf(' 7. **Closing.**'));
    const hold = CONFIG_KEYS.find((k) => k.name === 'RECOVERY_CLOCK_HOLD_MS');
    assert.ok(hold && hold.default > 0);
    assert.match(step6, /`RECOVERY_CLOCK_HOLD_MS` at most/);
    assert.ok(step6.includes(`${hold.default / 1000} s by default`), 'the default hold');
    assert.match(step6, /`running = None` even from ply 2 on/);
    assert.match(step6, /the opponent receives a `GameSnapshot` it did not ask for/);
    assert.doesNotMatch(step6, /the player's clock keeps running and/);
    assert.match(section('Clocks'), /`running` is the colour whose clock is running [^*]*restored after a restart waits for its player, `RECOVERY_CLOCK_HOLD_MS` at most/);
    const snapshot = flat(docs.split('\n').find((l) => l.startsWith('`0xA0`')) || '');
    assert.match(snapshot, /also to the opponent when the held clock of a game restored after a restart starts/);
    assert.match(schema.messages.find((m) => m.name === 'GameSnapshot').doc, /held clock/);
});

test('generation is deterministic', () => {
    assert.equal(generateCodec(), generateCodec());
    assert.equal(generateDocs(), generateDocs());
    assert.equal(renderVectors(), renderVectors());
    assert.ok(generateCodec().startsWith('// Scacelith realtime protocol codec: GENERATED'));
    assert.match(generateCodec(), /do not edit/);
});

function variant(change) {
    const s = structuredClone({
        PROTOCOL_VERSION: schema.PROTOCOL_VERSION, PROTOCOL_MIN: schema.PROTOCOL_MIN, WS_SUBPROTOCOL: schema.WS_SUBPROTOCOL,
        enums: schema.enums, MoveFlag: schema.MoveFlag, CloseCode: schema.CloseCode, structs: schema.structs, messages: schema.messages,
    });
    change(s);
    return s;
}

test('the generator refuses enum values that do not fit the u8 encoding', () => {
    assert.throws(() => generateCodec(variant((s) => { s.enums.ErrorCode.Huge = 256; })), /enum ErrorCode\.Huge = 256: enums travel as u8, every value must be an integer in 0\.\.255/);
    assert.throws(() => buildModel(variant((s) => { s.enums.Color.Neg = -1; })), /enum Color\.Neg = -1/);
    assert.throws(() => buildModel(variant((s) => { s.enums.Color.Half = 1.5; })), /enum Color\.Half = 1\.5/);
    assert.throws(() => generateDocs(variant((s) => { s.enums.ErrorCode.ProtocolViolation = 300; })), /enums travel as u8/);
});

test('the generator refuses other schemas it cannot encode', () => {
    const cases = [
        [(s) => { s.messages.push({ id: 0x30, name: 'NoSeq', dir: 'c2s', fields: [['game', 'id53']] }); }, /start with \['seq', 'u32'\]/],
        [(s) => { s.messages.push({ id: 0x30, name: 'X', dir: 's2c', fields: [] }); }, /ids 0x01-0x7F are client->server/],
        [(s) => { s.messages.push({ id: 0x20, name: 'Dup', dir: 'c2s', fields: [['seq', 'u32']] }); }, /duplicate id 0x20/],
        [(s) => { s.messages.push({ id: 0xb0, name: 'T', dir: 's2c', fields: [['type', 'u8']] }); }, /"type" is reserved/],
        [(s) => { s.messages.push({ id: 0xb0, name: 'S', dir: 's2c', fields: [['s', 'str8', { max: 300 }]] }); }, /str8 bounds/],
        [(s) => { s.messages.push({ id: 0xb0, name: 'L', dir: 's2c', fields: [['l', 'list16:u8', { max: 70000 }]] }); }, /list16 max/],
        [(s) => { s.messages.push({ id: 0xb0, name: 'LL', dir: 's2c', fields: [['l', 'list16:list16:u8']] }); }, /lists of lists/],
        [(s) => { s.messages.push({ id: 0xb0, name: 'U', dir: 's2c', fields: [['x', 'u64']] }); }, /unknown type u64/],
        [(s) => { s.messages.push({ id: 0xb0, name: 'E', dir: 's2c', fields: [['x', 'enum:Nope']] }); }, /unknown enum Nope/],
        [(s) => { s.messages.push({ id: 0xb0, name: 'B', dir: 's2c', fields: [['x', 'u8', { max: 300 }]] }); }, /bounds outside u8/],
        [(s) => { s.messages.push({ id: 0xb0, name: 'D', dir: 's2c', fields: [['x', 'u8'], ['x', 'u8']] }); }, /duplicate field x/],
        [(s) => { s.structs.Loop = [['self', 'struct:Loop']]; }, /recursive/],
        [(s) => { s.messages.push({ id: 0xb0, name: 'Ping', dir: 's2c', fields: [] }); }, /two s2c messages have this name|shared by one c2s and one s2c message only/],
        [(s) => { s.PROTOCOL_MIN = s.PROTOCOL_VERSION + 1; }, /PROTOCOL_MIN is above PROTOCOL_VERSION/],
        [(s) => { s.WS_SUBPROTOCOL = 'scacelith"v1'; }, /WS_SUBPROTOCOL must be an RFC 7230 token/],
        [(s) => { s.WS_SUBPROTOCOL = 'scacelith\\v1'; }, /WS_SUBPROTOCOL must be an RFC 7230 token/],
    ];
    for (const [change, re] of cases) assert.throws(() => generateCodec(variant(change)), re);
});
