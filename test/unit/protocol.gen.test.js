// The generator: committed generated files are up to date, and schemas that cannot be encoded
// faithfully are refused with a clear message.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import * as schema from '../../src/protocol/schema.js';
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
    ];
    for (const [change, re] of cases) assert.throws(() => generateCodec(variant(change)), re);
});
