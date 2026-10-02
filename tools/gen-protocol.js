#!/usr/bin/env node
// Protocol code generator (JavaScript side): turns src/protocol/schema.js into
//
//   src/protocol/codec.gen.js   specialised straight-line encoders/decoders (no schema interpretation
//                               at run time), with the API of docs/DESIGN.md section 5.1
//   docs/PROTOCOL.md            the protocol reference (generated tables + the prose templates below)
//
// and then runs tools/gen-protocol-vectors.js (golden vectors) and, when present,
// tools/gen-protocol-cpp.js (the game client's C++ codec) and tools/gen-cpp-test-vectors.js (its
// test vectors, tests/data/net-protocol-vectors.json), so `npm run gen:protocol` refreshes every
// generated protocol file.
//
//   node tools/gen-protocol.js           write the generated files
//   node tools/gen-protocol.js --check   exit 1 when a committed generated file differs from what
//                                        the schema produces (CI / tests)
//   node tools/gen-protocol.js --bench   compare the interpreted reference codec and the generated one
//
// The generator refuses a schema it cannot encode faithfully (enum values outside 0..255, a
// client->server message not starting with `seq`, string bounds above 255 bytes...).

import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';
import * as defaultSchema from '../src/protocol/schema.js';
import { computeSchemaHash } from '../src/protocol/schema-hash.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(here, '..');
export const CODEC_PATH = path.join(ROOT, 'src/protocol/codec.gen.js');
export const DOCS_PATH = path.join(ROOT, 'docs/PROTOCOL.md');

// ---- schema model ------------------------------------------------------------------------------

const FIXED = { u8: 1, bool: 1, enum: 1, u16: 2, u32: 4, i32: 4, f64: 8, id53: 8 };
const INT_RANGE = { u8: [0, 0xff], u16: [0, 0xffff], u32: [0, 0xffffffff], i32: [-0x80000000, 0x7fffffff] };
const IDENT = /^[A-Za-z_$][A-Za-z0-9_$]*$/;

/**
 * Validates a schema module and resolves its types.
 * @param {object} s schema module ({ PROTOCOL_VERSION, PROTOCOL_MIN, WS_SUBPROTOCOL, enums, MoveFlag, GestureFlag, CloseCode, structs, messages })
 * @returns {{ enums: object, structs: Map<string, object>, messages: object[] }}
 * @throws {Error} with a clear message when the schema cannot be generated
 */
export function buildModel(s = defaultSchema) {
    const bad = (msg) => { throw new Error(`schema: ${msg}`); };
    for (const k of ['PROTOCOL_VERSION', 'PROTOCOL_MIN']) {
        if (!Number.isInteger(s[k]) || s[k] < 0 || s[k] > 0xffff) bad(`${k} must be a u16`);
    }
    if (s.PROTOCOL_MIN > s.PROTOCOL_VERSION) bad('PROTOCOL_MIN is above PROTOCOL_VERSION');
    // An RFC 7230 token, as Sec-WebSocket-Protocol requires: no double quote or backslash (the C++
    // generator writes it into a string literal).
    if (typeof s.WS_SUBPROTOCOL !== 'string' || !/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(s.WS_SUBPROTOCOL)) bad('WS_SUBPROTOCOL must be an RFC 7230 token');

    const enums = {};
    for (const [en, e] of Object.entries(s.enums)) {
        if (!IDENT.test(en)) bad(`enum name ${en}`);
        const values = [];
        for (const [k, v] of Object.entries(e)) {
            if (!IDENT.test(k)) bad(`enum ${en}: value name ${k}`);
            if (!Number.isInteger(v) || v < 0 || v > 255) {
                bad(`enum ${en}.${k} = ${v}: enums travel as u8, every value must be an integer in 0..255`);
            }
            values.push(v);
        }
        if (!values.length) bad(`enum ${en} is empty`);
        enums[en] = { name: en, entries: Object.entries(e), values: [...new Set(values)].sort((a, b) => a - b) };
    }

    const structs = new Map();
    const resolving = new Set();
    function parseType(t, where) {
        if (typeof t !== 'string') bad(`${where}: type must be a string`);
        if (t.startsWith('list16:')) {
            const item = parseType(t.slice(7), where);
            if (item.kind === 'list') bad(`${where}: lists of lists are not supported`);
            return { kind: 'list', item, size: -1 };
        }
        if (t.startsWith('struct:')) return resolveStruct(t.slice(7), where);
        if (t.startsWith('enum:')) {
            const name = t.slice(5);
            if (!enums[name]) bad(`${where}: unknown enum ${name}`);
            return { kind: 'enum', name, e: enums[name], size: 1 };
        }
        if (t === 'str8') return { kind: 'str8', size: -1 };
        if (FIXED[t] === undefined || t === 'enum') bad(`${where}: unknown type ${t}`);
        return { kind: t, size: FIXED[t] };
    }
    function parseField(f, where, names) {
        if (!Array.isArray(f) || f.length < 2 || f.length > 3) bad(`${where}: a field is [name, type, opts?]`);
        const [name, type, opts = {}] = f;
        if (!IDENT.test(name)) bad(`${where}: field name ${name}`);
        if (name === 'type') bad(`${where}: "type" is reserved (decoded objects carry the message type in it)`);
        if (names.has(name)) bad(`${where}: duplicate field ${name}`);
        names.add(name);
        const w = `${where}.${name}`;
        const t = parseType(type, w);
        for (const k of Object.keys(opts)) if (k !== 'min' && k !== 'max') bad(`${w}: unknown option ${k}`);
        for (const k of ['min', 'max']) if (opts[k] !== undefined && !Number.isInteger(opts[k])) bad(`${w}: opts.${k} must be an integer`);
        if (opts.min !== undefined && opts.max !== undefined && opts.min > opts.max) bad(`${w}: min > max`);
        if (INT_RANGE[t.kind]) {
            const [lo, hi] = INT_RANGE[t.kind];
            if ((opts.min !== undefined && (opts.min < lo || opts.min > hi)) || (opts.max !== undefined && (opts.max < lo || opts.max > hi))) bad(`${w}: bounds outside ${t.kind}`);
        } else if (t.kind === 'str8') {
            if ((opts.max !== undefined && (opts.max < 0 || opts.max > 255)) || (opts.min !== undefined && (opts.min < 0 || opts.min > 255))) bad(`${w}: str8 bounds must be within 0..255 bytes`);
        } else if (t.kind === 'list') {
            if (opts.min !== undefined) bad(`${w}: lists take opts.max only`);
            if (opts.max !== undefined && (opts.max < 0 || opts.max > 0xffff)) bad(`${w}: list16 max must be within 0..65535`);
        } else if (opts.min !== undefined || opts.max !== undefined) bad(`${w}: ${t.kind} takes no bounds`);
        return { name, t, opts };
    }
    function resolveStruct(name, where) {
        if (structs.has(name)) return structs.get(name);
        if (!s.structs || !s.structs[name]) bad(`${where}: unknown struct ${name}`);
        if (resolving.has(name)) bad(`struct ${name} is recursive`);
        resolving.add(name);
        const names = new Set();
        const fields = s.structs[name].map((f) => parseField(f, `struct ${name}`, names));
        resolving.delete(name);
        const size = fields.every((f) => f.t.size >= 0) ? fields.reduce((a, f) => a + f.t.size, 0) : -1;
        const st = { kind: 'struct', name, fields, size };
        structs.set(name, st);
        return st;
    }
    for (const name of Object.keys(s.structs || {})) resolveStruct(name, `struct ${name}`);

    const ids = new Set();
    const perDir = { c2s: new Set(), s2c: new Set() };
    const count = new Map();
    for (const m of s.messages) count.set(m.name, (count.get(m.name) || 0) + 1);
    const messages = s.messages.map((m) => {
        const where = `message ${m.name}`;
        if (!IDENT.test(m.name)) bad(`${where}: name`);
        if (!Number.isInteger(m.id) || m.id < 1 || m.id > 255) bad(`${where}: id must be 1..255`);
        if (ids.has(m.id)) bad(`${where}: duplicate id 0x${m.id.toString(16)}`);
        ids.add(m.id);
        if (m.dir !== 'c2s' && m.dir !== 's2c') bad(`${where}: dir must be c2s or s2c`);
        if ((m.dir === 'c2s') !== (m.id <= 0x7f)) bad(`${where}: ids 0x01-0x7F are client->server, 0x80-0xFF server->client`);
        if (perDir[m.dir].has(m.name)) bad(`${where}: two ${m.dir} messages have this name`);
        perDir[m.dir].add(m.name);
        if (count.get(m.name) > 2) bad(`${where}: a name can be shared by one c2s and one s2c message only`);
        const names = new Set();
        const fields = m.fields.map((f) => parseField(f, where, names));
        if (m.dir === 'c2s' && (!fields.length || fields[0].name !== 'seq' || fields[0].t.kind !== 'u32' || Object.keys(fields[0].opts).length)) {
            bad(`${where}: client->server messages start with ['seq', 'u32']`);
        }
        const key = count.get(m.name) > 1 ? (m.dir === 'c2s' ? 'C_' : 'S_') + m.name : m.name;
        const fixed = fields.every((f) => f.t.size >= 0);
        const size = fixed ? 1 + fields.reduce((a, f) => a + f.t.size, 0) : -1;
        return { id: m.id, name: m.name, key, dir: m.dir, doc: m.doc || '', fields, size, src: m };
    });
    return { enums, structs, messages };
}

// Minimum encoded size of a type (fixed part: strings empty, lists empty).
function minSize(t) {
    if (t.size >= 0) return t.size;
    if (t.kind === 'str8') return 1;
    if (t.kind === 'list') return 2;
    return t.fields.reduce((a, f) => a + minSize(f.t), 0);
}

// ---- small code-building helpers -----------------------------------------------------------------

// Statement moving the running offset `o` to `cur + d`.
const setO = (cur, d) => (cur.base === 'o' ? (cur.k + d ? `o += ${cur.k + d};` : '') : `o = ${at(cur, d)};`);
const hex = (v, w = 2) => '0x' + v.toString(16).toUpperCase().padStart(w, '0');
const q = (s) => `'${String(s).replace(/\\/g, '\\\\').replace(/'/g, "\\'")}'`;
const at = (cur, d = 0) => {
    const k = cur.k + d;
    if (!cur.base) return String(k);
    return k ? `${cur.base} + ${k}` : cur.base;
};
function jsObj(o, indent = '') {
    const parts = Object.entries(o).map(([k, v]) => `${k}: ${typeof v === 'string' ? q(v) : v}`);
    const one = `{ ${parts.join(', ')} }`;
    if (one.length + indent.length <= 110) return one;
    const lines = [];
    let line = indent + '    ';
    for (const p of parts) {
        if (line.length + p.length + 2 > 110 && line.trim()) { lines.push(line.trimEnd()); line = indent + '    '; }
        line += p + ', ';
    }
    if (line.trim()) lines.push(line.trimEnd());
    return `{\n${lines.join('\n')}\n${indent}}`;
}

class Out {
    constructor() { this.lines = []; this.ind = 0; this.n = 0; }
    line(s = '') { this.lines.push(s ? '    '.repeat(this.ind) + s : ''); }
    tmp(p = 'v') { return `${p}${++this.n}`; }
    text() { return this.lines.join('\n'); }
}

// Read expressions (b = buffer, P = offset expression).
const rd = {
    u8: (P) => `b[${P}]`,
    u16: (P, P1) => `b[${P}] | b[${P1}] << 8`,
    u32: (P, P1, P2, P3) => `(b[${P}] | b[${P1}] << 8 | b[${P2}] << 16 | b[${P3}] << 24) >>> 0`,
    i32: (P, P1, P2, P3) => `b[${P}] | b[${P1}] << 8 | b[${P2}] << 16 | b[${P3}] << 24`,
};

function enumCheckDecode(e, x) {
    const vs = e.values;
    const lo = vs[0], hi = vs[vs.length - 1];
    if (hi - lo + 1 === vs.length) return lo === 0 ? `${x} > ${hi}` : `${x} < ${lo} || ${x} > ${hi}`;
    return `EN_${e.name}[${x}] === 0`;
}
function enumCheckEncode(e, x) {
    const vs = e.values;
    const lo = vs[0], hi = vs[vs.length - 1];
    const head = `(${x} & 255) !== ${x}`;
    if (hi - lo + 1 === vs.length) return lo === 0 ? `${head} || ${x} > ${hi}` : `${head} || ${x} < ${lo} || ${x} > ${hi}`;
    return `${head} || EN_${e.name}[${x}] === 0`;
}
function enumNeedsTable(e) { return e.values[e.values.length - 1] - e.values[0] + 1 !== e.values.length; }

// ---- decoder generation --------------------------------------------------------------------------

// Flattens a field sequence (structs inline) into leaves and returns the tree used to rebuild the
// decoded object. reason: the name used in ProtocolError reasons (as the interpreted codec does).
function flatten(fields, prefix, leaves) {
    return fields.map((f) => {
        const reason = prefix + f.name;
        if (f.t.kind === 'struct') return { name: f.name, children: flatten(f.t.fields, reason + '.', leaves) };
        const leaf = { f, reason, v: null };
        leaves.push(leaf);
        return { name: f.name, leaf };
    });
}
function literal(tree, indent) {
    const parts = tree.map((n) => `${n.name}: ${n.children ? literal(n.children, indent) : n.leaf.v}`);
    return `{ ${parts.join(', ')} }`;
}

// Emits decoding of a field sequence starting at `cur` ({ base: 'o' | null, k }). Returns the tree
// and the cursor after it. `checked`: the caller already verified that the fixed part fits (fully
// fixed messages check the exact length once).
function emitDecodeSeq(out, fields, prefix, cur, checked) {
    const leaves = [];
    const tree = flatten(fields, prefix, leaves);
    let i = 0;
    while (i < leaves.length) {
        // One run: fixed-size leaves up to (and including the length prefix of) the next variable one.
        let j = i, runLen = 0;
        for (; j < leaves.length; j++) {
            const t = leaves[j].f.t;
            if (t.size >= 0) runLen += t.size;
            else { runLen += t.kind === 'str8' ? 1 : 2; break; }
        }
        if (!checked && runLen > 0) out.line(`if (${at(cur, runLen)} > n) fail('truncated');`);
        const end = Math.min(j, leaves.length - 1);
        for (let x = i; x <= end; x++) cur = emitDecodeLeaf(out, leaves[x], cur, checked);
        i = end + 1;
    }
    return { tree, cur };
}

function emitIntChecks(out, x, t, opts, reason, failFn) {
    const [lo, hi] = INT_RANGE[t.kind];
    if (opts.min !== undefined && opts.min > lo) out.line(`if (${x} < ${opts.min}) ${failFn}(${q(reason + ' below min')});`);
    if (opts.max !== undefined && opts.max < hi) out.line(`if (${x} > ${opts.max}) ${failFn}(${q(reason + ' above max')});`);
}

// Emits the read of a fixed-size scalar at cursor `cur`; returns the value expression.
function emitDecodeScalar(out, t, opts, reason, cur) {
    const P = (d) => at(cur, d);
    const v = out.tmp();
    switch (t.kind) {
        case 'u8': case 'u16': case 'u32': case 'i32':
            out.line(`const ${v} = ${rd[t.kind](P(0), P(1), P(2), P(3))};`);
            emitIntChecks(out, v, t, opts, reason, 'fail');
            return v;
        case 'bool':
            out.line(`const ${v} = b[${P(0)}];`);
            out.line(`if (${v} > 1) fail(${q(reason + ' not a bool')});`);
            return `${v} === 1`;
        case 'enum':
            out.line(`const ${v} = b[${P(0)}];`);
            out.line(`if (${enumCheckDecode(t.e, v)}) fail(${q(`${reason} not a ${t.name}`)});`);
            return v;
        case 'f64':
            out.line(`const ${v} = rf64(b, ${P(0)});`);
            out.line(`if (${v} - ${v} !== 0) fail(${q(reason + ' not finite')});`);
            return v;
        case 'id53': {
            const hi = out.tmp('h');
            out.line(`const ${hi} = ${rd.u32(P(4), P(5), P(6), P(7))};`);
            out.line(`if (${hi} >= 0x200000) fail(${q(reason + ' above 2^53')});`);
            out.line(`const ${v} = ${hi} * 4294967296 + (${rd.u32(P(0), P(1), P(2), P(3))});`);
            return v;
        }
    }
    throw new Error(`gen: scalar ${t.kind}`);
}

function emitDecodeLeaf(out, leaf, cur, checked) {
    const { f, reason } = leaf;
    const t = f.t;
    if (t.size >= 0) {
        leaf.v = emitDecodeScalar(out, t, f.opts, reason, cur);
        return { base: cur.base, k: cur.k + t.size };
    }
    if (t.kind === 'str8') {
        const len = out.tmp('l');
        out.line(`const ${len} = b[${at(cur)}];`);
        const conds = [];
        if (f.opts.max !== undefined && f.opts.max < 255) conds.push(`${len} > ${f.opts.max}`);
        if (f.opts.min !== undefined && f.opts.min > 0) conds.push(`${len} < ${f.opts.min}`);
        if (conds.length) out.line(`if (${conds.join(' || ')}) fail(${q(reason + ' bad length')});`);
        { const st = setO(cur, 1); if (st) out.line(st); }
        out.line(`if (o + ${len} > n) fail('truncated');`);
        const v = out.tmp();
        out.line(`const ${v} = ${len} === 0 ? '' : rstr(b, o, o + ${len}, ${q(reason)});`);
        out.line(`o += ${len};`);
        leaf.v = v;
        return { base: 'o', k: 0 };
    }
    // list16
    const cnt = out.tmp('c');
    out.line(`const ${cnt} = ${rd.u16(at(cur), at(cur, 1))};`);
    if (f.opts.max !== undefined && f.opts.max < 0xffff) out.line(`if (${cnt} > ${f.opts.max}) fail(${q(reason + ' too long')});`);
    { const st = setO(cur, 2); if (st) out.line(st); }
    const item = t.item;
    const arr = out.tmp('a');
    const idx = out.tmp('i');
    out.line(`const ${arr} = [];`);
    if (item.size >= 0) {
        out.line(`if (o + ${cnt} * ${item.size} > n) fail('truncated');`);
        out.line(`for (let ${idx} = 0; ${idx} < ${cnt}; ${idx}++, o += ${item.size}) {`);
        out.ind++;
        const expr = emitDecodeItem(out, item, reason, { base: 'o', k: 0 }, true).expr;
        out.line(`${arr}.push(${expr});`);
        out.ind--;
        out.line('}');
    } else {
        out.line(`for (let ${idx} = 0; ${idx} < ${cnt}; ${idx}++) {`);
        out.ind++;
        const r = emitDecodeItem(out, item, reason, { base: 'o', k: 0 }, false);
        if (r.cur.k) out.line(`o = ${at(r.cur)};`);
        out.line(`${arr}.push(${r.expr});`);
        out.ind--;
        out.line('}');
    }
    leaf.v = arr;
    return { base: 'o', k: 0 };
}

// A list item: a struct (fields named "<list>.<field>" in reasons) or a scalar (named "<list>").
function emitDecodeItem(out, item, listReason, cur, checked) {
    if (item.kind === 'struct') {
        const r = emitDecodeSeq(out, item.fields, listReason + '.', cur, checked);
        return { expr: literal(r.tree), cur: r.cur };
    }
    const fields = [{ name: 'item', t: item, opts: {} }];
    const leaves = [];
    const tree = flatten(fields, '', leaves);
    leaves[0].reason = listReason;
    if (item.size >= 0) {
        if (!checked) out.line(`if (${at(cur, item.size)} > n) fail('truncated');`);
        const v = emitDecodeScalar(out, item, {}, listReason, cur);
        return { expr: v, cur: { base: cur.base, k: cur.k + item.size } };
    }
    // str8 item
    if (!checked) out.line(`if (${at(cur, 1)} > n) fail('truncated');`);
    const c = emitDecodeLeaf(out, leaves[0], cur, checked);
    return { expr: tree[0].leaf.v, cur: c };
}

function genDecoder(m) {
    const out = new Out();
    out.ind = 1;
    const fixed = m.size >= 0;
    // A fully fixed message is checked once against its exact size; the others per run.
    if (fixed) out.line(`if (n !== ${m.size}) fail(n < ${m.size} ? 'truncated' : 'trailing bytes');`);
    else out.line('let o = 0;');
    const r = emitDecodeSeq(out, m.fields, '', { base: null, k: 1 }, fixed);
    if (!fixed) out.line(`if (${at(r.cur)} !== n) fail('trailing bytes');`);
    const obj = literal([{ name: 'type', leaf: { v: hex(m.id) } }, ...r.tree]);
    out.line(`return ${obj};`);
    return `// ${hex(m.id)} ${m.key} (${m.dir})\nfunction d_${m.key}(b, n) {\n${out.text()}\n}`;
}

// ---- encoder generation --------------------------------------------------------------------------

// Pass 1 collects every variable-size part (strings, lists) to size the buffer exactly; pass 2
// validates and writes the fields in schema order.
function genEncoder(m) {
    const out = new Out();
    out.ind = 1;
    out.line(`if (m === null || typeof m !== 'object') efail('message not an object');`);
    const sizeParts = [];
    let fixedBytes = 1;
    const helpers = [];

    // pass 1: bind every field to a local, measure the variable parts.
    function bind(fields, src, prefix) {
        return fields.map((f) => {
            const reason = prefix + f.name;
            const expr = `${src}.${f.name}`;
            const t = f.t;
            if (t.kind === 'struct') {
                const v = out.tmp('s');
                out.line(`const ${v} = ${expr} ?? NO_OBJ;`);
                return { f, reason, v, children: bind(t.fields, v, reason + '.') };
            }
            const v = out.tmp();
            if (t.kind === 'str8') {
                const L = out.tmp('L');
                out.line(`const ${v} = ${expr} ?? '';`);
                out.line(`if (typeof ${v} !== 'string') efail(${q(reason + ' not a string')});`);
                out.line(`const ${L} = slen(${v});`);
                out.line(`if (${L} < 0) efail(${q(reason + ' contains NUL')});`);
                const conds = [`${L} > ${f.opts.max ?? 255}`];
                if (f.opts.min) conds.push(`${L} < ${f.opts.min}`);
                out.line(`if (${conds.join(' || ')}) efail(${q(reason + ' bad length')});`);
                fixedBytes += 1;
                sizeParts.push(L);
                return { f, reason, v, L };
            }
            if (t.kind === 'list') {
                out.line(`const ${v} = ${expr} ?? NO_ARR;`);
                out.line(`if (!Array.isArray(${v})) efail(${q(reason + ' not an array')});`);
                out.line(`if (${v}.length > ${Math.min(f.opts.max ?? 0xffff, 0xffff)}) efail(${q(reason + ' too long')});`);
                fixedBytes += 2;
                if (t.item.size >= 0) sizeParts.push(`${v}.length * ${t.item.size}`);
                else {
                    const S = out.tmp('S');
                    out.line(`let ${S} = 0;`);
                    out.line(`for (let i = 0; i < ${v}.length; i++) ${S} += ${itemSizeFn(t.item, reason, helpers)}(${v}[i]);`);
                    sizeParts.push(S);
                }
                return { f, reason, v };
            }
            fixedBytes += t.size;
            out.line(`const ${v} = ${expr};`);
            return { f, reason, v };
        });
    }
    const bound = bind(m.fields, 'm', '');
    const sizeExpr = [String(fixedBytes), ...sizeParts].join(' + ');
    out.line(`const b = Buffer.allocUnsafe(${sizeExpr});`);
    out.line(`b[0] = ${hex(m.id)};`);
    let cur = { base: null, k: 1 };
    function write(list) {
        for (const x of list) {
            const t = x.f.t;
            if (t.kind === 'struct') { write(x.children); continue; }
            if (t.size >= 0) {
                emitEncodeScalar(out, t, x.f.opts, x.reason, x.v, cur);
                cur = { base: cur.base, k: cur.k + t.size };
            } else if (t.kind === 'str8') {
                out.line(`b[${at(cur)}] = ${x.L};`);
                { const st = setO(cur, 1); if (st) out.line(st); }
                out.line(`if (${x.L} !== 0) { wstr(b, o, ${x.v}, ${x.L}); o += ${x.L}; }`);
                cur = { base: 'o', k: 0 };
            } else {
                const cnt = out.tmp('c');
                out.line(`const ${cnt} = ${x.v}.length;`);
                out.line(`b[${at(cur)}] = ${cnt}; b[${at(cur, 1)}] = ${cnt} >>> 8;`);
                { const st = setO(cur, 2); if (st) out.line(st); }
                const idx = out.tmp('i');
                out.line(`for (let ${idx} = 0; ${idx} < ${cnt}; ${idx}++) {`);
                out.ind++;
                if (t.item.size >= 0 && t.item.kind === 'struct') {
                    const it = out.tmp('s');
                    out.line(`const ${it} = ${x.v}[${idx}] ?? NO_OBJ;`);
                    let c = { base: 'o', k: 0 };
                    const walk = (fields, src, prefix) => {
                        for (const f of fields) {
                            if (f.t.kind === 'struct') {
                                const sv = out.tmp('s');
                                out.line(`const ${sv} = ${src}.${f.name} ?? NO_OBJ;`);
                                walk(f.t.fields, sv, prefix + f.name + '.');
                                continue;
                            }
                            const v = out.tmp();
                            out.line(`const ${v} = ${src}.${f.name};`);
                            emitEncodeScalar(out, f.t, f.opts, prefix + f.name, v, c);
                            c = { base: 'o', k: c.k + f.t.size };
                        }
                    };
                    walk(t.item.fields, it, x.reason + '.');
                    out.line(`o += ${t.item.size};`);
                } else if (t.item.size >= 0) {
                    const v = out.tmp();
                    out.line(`const ${v} = ${x.v}[${idx}];`);
                    emitEncodeScalar(out, t.item, {}, x.reason, v, { base: 'o', k: 0 });
                    out.line(`o += ${t.item.size};`);
                } else {
                    out.line(`o = ${itemWriteFn(t.item, x.reason, helpers)}(b, o, ${x.v}[${idx}]);`);
                }
                out.ind--;
                out.line('}');
                cur = { base: 'o', k: 0 };
            }
        }
    }
    if (m.size < 0) out.line('let o = 0;');
    write(bound);
    out.line('return b;');
    return helpers.join('\n\n') + (helpers.length ? '\n\n' : '') +
        `// ${hex(m.id)} ${m.key} (${m.dir})\nfunction e_${m.key}(m) {\n${out.text()}\n}`;
}

function emitEncodeScalar(out, t, opts, reason, v, cur) {
    const P = (d) => at(cur, d);
    switch (t.kind) {
        case 'u8':
            out.line(`if ((${v} & 255) !== ${v}) efail(${q(reason + ' out of range')});`);
            emitIntChecks(out, v, t, opts, reason, 'efail');
            out.line(`b[${P(0)}] = ${v};`);
            return;
        case 'u16':
            out.line(`if ((${v} & 65535) !== ${v}) efail(${q(reason + ' out of range')});`);
            emitIntChecks(out, v, t, opts, reason, 'efail');
            out.line(`b[${P(0)}] = ${v}; b[${P(1)}] = ${v} >>> 8;`);
            return;
        case 'u32': case 'i32':
            out.line(`if ((${v} ${t.kind === 'u32' ? '>>> 0' : '| 0'}) !== ${v}) efail(${q(reason + ' out of range')});`);
            emitIntChecks(out, v, t, opts, reason, 'efail');
            out.line(`b[${P(0)}] = ${v}; b[${P(1)}] = ${v} >>> 8; b[${P(2)}] = ${v} >>> 16; b[${P(3)}] = ${v} >>> 24;`);
            return;
        case 'bool':
            out.line(`b[${P(0)}] = ${v} ? 1 : 0;`);
            return;
        case 'enum':
            out.line(`if (${enumCheckEncode(t.e, v)}) efail(${q(`${reason} not a ${t.name}`)});`);
            out.line(`b[${P(0)}] = ${v};`);
            return;
        case 'f64': {
            const x = out.tmp('d');
            out.line(`const ${x} = Number(${v}) || 0;`);
            out.line(`if (${x} - ${x} !== 0) efail(${q(reason + ' not finite')});`);
            out.line(`wf64(b, ${P(0)}, ${x});`);
            return;
        }
        case 'id53': {
            const x = out.tmp('g');
            const lo = out.tmp('lo');
            const hi = out.tmp('hi');
            out.line(`const ${x} = ${v} || 0;`);
            out.line(`if (!Number.isSafeInteger(${x}) || ${x} < 0) efail(${q(reason + ' not an id53')});`);
            out.line(`const ${lo} = ${x} >>> 0, ${hi} = (${x} - ${lo}) / 4294967296;`);
            out.line(`b[${P(0)}] = ${lo}; b[${P(1)}] = ${lo} >>> 8; b[${P(2)}] = ${lo} >>> 16; b[${P(3)}] = ${lo} >>> 24;`);
            out.line(`b[${P(4)}] = ${hi}; b[${P(5)}] = ${hi} >>> 8; b[${P(6)}] = ${hi} >>> 16; b[${P(7)}] = ${hi} >>> 24;`);
            return;
        }
    }
    throw new Error(`gen: scalar ${t.kind}`);
}

// Variable-size list items (a struct holding a string or a list, or a list of strings) go through
// per-item helpers: z_<id>(v) -> encoded size (validates), w_<id>(b, o, v) -> next offset.
let helperSeq = 0;
function itemSizeFn(item, reason, helpers) {
    const name = `z${++helperSeq}`;
    const out = new Out();
    out.ind = 1;
    if (item.kind === 'str8') {
        out.line(`if (typeof v !== 'string') efail(${q(reason + ' not a string')});`);
        out.line('const L = slen(v);');
        out.line(`if (L < 0) efail(${q(reason + ' contains NUL')});`);
        out.line(`if (L > 255) efail(${q(reason + ' bad length')});`);
        out.line('return 1 + L;');
    } else {
        out.line('const s = v ?? NO_OBJ;');
        out.line(`let n = ${minSize(item)};`);
        const walk = (fields, src, prefix) => {
            for (const f of fields) {
                const r = prefix + f.name;
                const e = `${src}.${f.name}`;
                if (f.t.kind === 'struct') {
                    const sv = out.tmp('s');
                    out.line(`const ${sv} = ${e} ?? NO_OBJ;`);
                    walk(f.t.fields, sv, r + '.');
                } else if (f.t.kind === 'str8') {
                    const x = out.tmp();
                    const L = out.tmp('L');
                    out.line(`const ${x} = ${e} ?? '';`);
                    out.line(`if (typeof ${x} !== 'string') efail(${q(r + ' not a string')});`);
                    out.line(`const ${L} = slen(${x});`);
                    out.line(`if (${L} < 0) efail(${q(r + ' contains NUL')});`);
                    const conds = [`${L} > ${f.opts.max ?? 255}`];
                    if (f.opts.min) conds.push(`${L} < ${f.opts.min}`);
                    out.line(`if (${conds.join(' || ')}) efail(${q(r + ' bad length')});`);
                    out.line(`n += ${L};`);
                } else if (f.t.kind === 'list') {
                    const x = out.tmp();
                    out.line(`const ${x} = ${e} ?? NO_ARR;`);
                    out.line(`if (!Array.isArray(${x})) efail(${q(r + ' not an array')});`);
                    out.line(`if (${x}.length > ${Math.min(f.opts.max ?? 0xffff, 0xffff)}) efail(${q(r + ' too long')});`);
                    if (f.t.item.size >= 0) out.line(`n += ${x}.length * ${f.t.item.size};`);
                    else out.line(`for (let i = 0; i < ${x}.length; i++) n += ${itemSizeFn(f.t.item, r, helpers)}(${x}[i]);`);
                }
            }
        };
        walk(item.fields, 's', reason + '.');
        out.line('return n;');
    }
    helpers.push(`function ${name}(v) {\n${out.text()}\n}`);
    return name;
}

function itemWriteFn(item, reason, helpers) {
    const name = `w${++helperSeq}`;
    const out = new Out();
    out.ind = 1;
    if (item.kind === 'str8') {
        out.line('const L = slen(v);');
        out.line('b[o] = L;');
        out.line('if (L !== 0) wstr(b, o + 1, v, L);');
        out.line('return o + 1 + L;');
    } else {
        out.line('const s = v ?? NO_OBJ;');
        let cur = { base: 'o', k: 0 };
        const walk = (fields, src, prefix) => {
            for (const f of fields) {
                const r = prefix + f.name;
                const e = `${src}.${f.name}`;
                const t = f.t;
                if (t.kind === 'struct') {
                    const sv = out.tmp('s');
                    out.line(`const ${sv} = ${e} ?? NO_OBJ;`);
                    walk(t.fields, sv, r + '.');
                } else if (t.size >= 0) {
                    const x = out.tmp();
                    out.line(`const ${x} = ${e};`);
                    emitEncodeScalar(out, t, f.opts, r, x, cur);
                    cur = { base: 'o', k: cur.k + t.size };
                } else if (t.kind === 'str8') {
                    const x = out.tmp();
                    const L = out.tmp('L');
                    out.line(`const ${x} = ${e} ?? '';`);
                    out.line(`const ${L} = slen(${x});`);
                    out.line(`b[${at(cur)}] = ${L};`);
                    { const st = setO(cur, 1); if (st) out.line(st); }
                    out.line(`if (${L} !== 0) { wstr(b, o, ${x}, ${L}); o += ${L}; }`);
                    cur = { base: 'o', k: 0 };
                } else {
                    const x = out.tmp();
                    out.line(`const ${x} = ${e} ?? NO_ARR;`);
                    out.line(`b[${at(cur)}] = ${x}.length; b[${at(cur, 1)}] = ${x}.length >>> 8;`);
                    { const st = setO(cur, 2); if (st) out.line(st); }
                    const idx = out.tmp('i');
                    out.line(`for (let ${idx} = 0; ${idx} < ${x}.length; ${idx}++) {`);
                    out.ind++;
                    if (t.item.size >= 0 && t.item.kind !== 'struct') {
                        const v = out.tmp();
                        out.line(`const ${v} = ${x}[${idx}];`);
                        emitEncodeScalar(out, t.item, {}, r, v, { base: 'o', k: 0 });
                        out.line(`o += ${t.item.size};`);
                    } else {
                        out.line(`o = ${itemWriteFn(t.item, r, helpers)}(b, o, ${x}[${idx}]);`);
                    }
                    out.ind--;
                    out.line('}');
                    cur = { base: 'o', k: 0 };
                }
            }
        };
        walk(item.fields, 's', reason + '.');
        out.line(`return ${at(cur)};`);
    }
    helpers.push(`function ${name}(b, o, v) {\n${out.text()}\n}`);
    return name;
}

// ---- codec module ----------------------------------------------------------------------------------

const CODEC_RUNTIME = `// ---- run-time helpers ----

/** Malformed message or value that cannot be encoded; \`reason\` says which rule failed. */
export class ProtocolError extends Error {
    constructor(reason) { super(\`protocol: \${reason}\`); this.reason = reason; }
}
ProtocolError.prototype.name = 'ProtocolError';

// Decoding errors carry no stack trace (the reason says everything, and capturing a stack costs
// more than decoding the message); encoding errors keep theirs (they are server bugs).
const TRIM_STACK = (() => {
    const d = Object.getOwnPropertyDescriptor(Error, 'stackTraceLimit');
    return !!d && d.writable === true && !Object.isFrozen(Error);
})();
function fail(reason) {
    if (TRIM_STACK) {
        const keep = Error.stackTraceLimit;
        Error.stackTraceLimit = 0;
        const e = new ProtocolError(reason);
        Error.stackTraceLimit = keep;
        throw e;
    }
    throw new ProtocolError(reason);
}
function efail(reason) { throw new ProtocolError(reason); }

const NO_OBJ = Object.freeze({});
const NO_ARR = Object.freeze([]);
const utf8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });
const F64 = new Float64Array(1);
const F8 = new Uint8Array(F64.buffer);
const LE = new Uint8Array(new Uint16Array([1]).buffer)[0] === 1;

function rf64(b, o) {
    if (LE) { F8[0] = b[o]; F8[1] = b[o + 1]; F8[2] = b[o + 2]; F8[3] = b[o + 3]; F8[4] = b[o + 4]; F8[5] = b[o + 5]; F8[6] = b[o + 6]; F8[7] = b[o + 7]; }
    else { F8[7] = b[o]; F8[6] = b[o + 1]; F8[5] = b[o + 2]; F8[4] = b[o + 3]; F8[3] = b[o + 4]; F8[2] = b[o + 5]; F8[1] = b[o + 6]; F8[0] = b[o + 7]; }
    return F64[0];
}
function wf64(b, o, v) {
    F64[0] = v;
    if (LE) { b[o] = F8[0]; b[o + 1] = F8[1]; b[o + 2] = F8[2]; b[o + 3] = F8[3]; b[o + 4] = F8[4]; b[o + 5] = F8[5]; b[o + 6] = F8[6]; b[o + 7] = F8[7]; }
    else { b[o] = F8[7]; b[o + 1] = F8[6]; b[o + 2] = F8[5]; b[o + 3] = F8[4]; b[o + 4] = F8[3]; b[o + 5] = F8[2]; b[o + 6] = F8[1]; b[o + 7] = F8[0]; }
}

// str8 payload b[s..e): no NUL, valid UTF-8 (fatal; a BOM is kept). ASCII takes the fast path.
function rstr(b, s, e, name) {
    let acc = 0;
    for (let i = s; i < e; i++) {
        const c = b[i];
        if (c === 0) fail(name + ' contains NUL');
        acc |= c;
    }
    if (acc < 0x80) return b.latin1Slice(s, e);
    try { return utf8.decode(b.subarray(s, e)); } catch { return fail(name + ' not UTF-8'); }
}

// UTF-8 byte length of a string as Buffer writes it (lone surrogates: 3 bytes), -1 if it has a NUL.
function slen(s) {
    const n = s.length;
    let len = n;
    for (let i = 0; i < n; i++) {
        const c = s.charCodeAt(i);
        if (c < 0x80) { if (c === 0) return -1; continue; }
        if (c < 0x800) len += 1;
        else if ((c & 0xfc00) === 0xd800 && i + 1 < n && (s.charCodeAt(i + 1) & 0xfc00) === 0xdc00) { len += 2; i++; }
        else len += 2;
    }
    return len;
}
function wstr(b, o, s, len) {
    if (len === s.length) { for (let i = 0; i < len; i++) b[o + i] = s.charCodeAt(i); }
    else b.write(s, o, len, 'utf8');
}

function toBuffer(x) {
    if (!x) fail('empty');
    if (x instanceof Uint8Array) return Buffer.from(x.buffer, x.byteOffset, x.byteLength);
    if (x instanceof ArrayBuffer) return Buffer.from(x);
    return fail('not a buffer');
}`;

/**
 * Generates the source of src/protocol/codec.gen.js.
 * @param {object} [s] schema module (defaults to src/protocol/schema.js)
 * @returns {string}
 */
export function generateCodec(s = defaultSchema) {
    helperSeq = 0;
    const model = buildModel(s);
    const hash = computeSchemaHash(s);
    const L = [];
    L.push('// Scacelith realtime protocol codec: GENERATED by tools/gen-protocol.js from src/protocol/schema.js,');
    L.push('// do not edit. Change schema.js and run `npm run gen:protocol` (CI runs it with --check).');
    L.push('//');
    L.push('// API (docs/DESIGN.md section 5.1): MSG, encode.<Name>(fields) -> Buffer, decode(buf, { dir }),');
    L.push('// ProtocolError{reason}, PROTOCOL_VERSION, PROTOCOL_MIN, WS_SUBPROTOCOL, SCHEMA_HASH, enums,');
    L.push('// MoveFlag, GestureFlag, CloseCode, isClientType, messageName. Wire format and validation rules:');
    L.push('// docs/PROTOCOL.md. Every function below is straight-line code specialised for one message:');
    L.push('// fixed parts at constant offsets, one bounds check per run of fixed-size fields, exact-size');
    L.push('// allocation on encode. Encoding validates like decoding, so the server cannot emit a frame');
    L.push('// that a client refuses.');
    L.push('/* eslint-disable */');
    L.push('');
    L.push(`export const PROTOCOL_VERSION = ${s.PROTOCOL_VERSION};`);
    L.push(`export const PROTOCOL_MIN = ${s.PROTOCOL_MIN};`);
    L.push(`export const WS_SUBPROTOCOL = ${q(s.WS_SUBPROTOCOL)};`);
    L.push(`export const SCHEMA_HASH = 0x${hash.toString(16).padStart(8, '0')};`);
    L.push('');
    L.push('export const enums = Object.freeze({');
    for (const [name, e] of Object.entries(s.enums)) L.push(`    ${name}: Object.freeze(${jsObj(e, '    ')}),`);
    L.push('});');
    L.push(`export const MoveFlag = Object.freeze(${jsObj(s.MoveFlag)});`);
    L.push(`export const GestureFlag = Object.freeze(${jsObj(s.GestureFlag)});`);
    L.push(`export const CloseCode = Object.freeze(${jsObj(s.CloseCode)});`);
    L.push('');
    L.push('export const MSG = Object.freeze({');
    for (const m of model.messages) L.push(`    ${m.key}: ${hex(m.id)},`);
    L.push('});');
    L.push('');
    L.push(CODEC_RUNTIME);
    L.push('');
    const tables = Object.values(model.enums).filter(enumNeedsTable);
    if (tables.length) {
        L.push('// Membership tables of the enums whose values are not contiguous.');
        L.push('function set8(values) { const t = new Uint8Array(256); for (const v of values) t[v] = 1; return t; }');
        for (const e of tables) L.push(`const EN_${e.name} = set8([${e.values.join(', ')}]);`);
        L.push('');
    }
    L.push('// Direction of each type byte: 0 unknown, 1 client->server, 2 server->client.');
    L.push('const DIR = new Uint8Array(256);');
    L.push('const NAMES = new Array(256).fill(null);');
    for (const m of model.messages) L.push(`DIR[${hex(m.id)}] = ${m.dir === 'c2s' ? 1 : 2}; NAMES[${hex(m.id)}] = ${q(m.key)};`);
    L.push('');
    L.push('/** True for client->server type bytes (0x01-0x7F). */');
    L.push('export function isClientType(t) { return t >= 0x01 && t <= 0x7f; }');
    L.push('');
    L.push('/** Name of a message type as used in MSG and encode (C_/S_ prefixes for shared names), or null. */');
    L.push("export function messageName(type) { return typeof type === 'number' ? NAMES[type] ?? null : null; }");
    L.push('');
    L.push('// ---- encoders ----');
    L.push('');
    for (const m of model.messages) { L.push(genEncoder(m)); L.push(''); }
    L.push('/** encode.<Name>(fields) -> Buffer of the exact size; throws ProtocolError when a value is invalid. */');
    L.push('export const encode = Object.freeze({');
    for (const m of model.messages) L.push(`    ${m.key}: e_${m.key},`);
    L.push('});');
    L.push('');
    L.push('// ---- decoders ----');
    L.push('');
    for (const m of model.messages) { L.push(genDecoder(m)); L.push(''); }
    L.push('/**');
    L.push(' * Decodes one message: { type, ...fields }. opts.dir (\'c2s\' | \'s2c\') refuses the other');
    L.push(' * direction\'s types. Throws ProtocolError (and nothing else) on any malformed input.');
    L.push(' * @param {Buffer|Uint8Array} buf');
    L.push(' * @param {{ dir?: \'c2s\' | \'s2c\' }} [opts]');
    L.push(' */');
    L.push('export function decode(buf, opts) {');
    L.push('    const b = buf instanceof Buffer ? buf : toBuffer(buf);');
    L.push('    const n = b.length;');
    L.push("    if (n === 0) fail('empty');");
    L.push('    const t = b[0];');
    L.push('    const d = DIR[t];');
    L.push("    if (d === 0) fail('unknown type');");
    L.push('    if (opts !== undefined && opts !== null) {');
    L.push('        const w = opts.dir;');
    L.push("        if (w && (w === 'c2s' ? 1 : w === 's2c' ? 2 : 0) !== d) fail('wrong direction');");
    L.push('    }');
    L.push('    switch (t) {');
    for (const m of model.messages) L.push(`        case ${hex(m.id)}: return d_${m.key}(b, n);`);
    L.push("        default: return fail('unknown type');");
    L.push('    }');
    L.push('}');
    L.push('');
    return L.join('\n');
}

// ---- documentation -------------------------------------------------------------------------------

function typeLabel(t) {
    let s;
    switch (t.kind) {
        case 'enum': s = `enum [${t.name}](#${t.name.toLowerCase()}) (u8)`; break;
        case 'struct': s = `struct [${t.name}](#${t.name.toLowerCase()})`; break;
        case 'list': s = `list16 of ${typeLabel(t.item)}`; break;
        default: s = t.kind;
    }
    return s;
}
function sizeLabel(t) {
    if (t.size >= 0) return String(t.size);
    if (t.kind === 'str8') return '1 + len';
    if (t.kind === 'list') return t.item.size >= 0 ? `2 + ${t.item.size}·n` : '2 + items';
    return `${minSize(t)}+`;
}
function limitLabel(t, opts) {
    const parts = [];
    if (t.kind === 'str8') {
        parts.push(`${opts.min ?? 0}..${opts.max ?? 255} bytes UTF-8, no NUL`);
    } else if (t.kind === 'list') {
        parts.push(`≤ ${opts.max ?? 65535} items`);
    } else if (t.kind === 'bool') parts.push('0 or 1');
    else if (t.kind === 'id53') parts.push('< 2^53');
    else if (t.kind === 'f64') parts.push('finite');
    else if (INT_RANGE[t.kind]) {
        const [lo, hi] = INT_RANGE[t.kind];
        const mn = opts.min ?? lo, mx = opts.max ?? hi;
        if (mn !== lo || mx !== hi) parts.push(`${mn}..${mx}${mx === 0x7fff ? ' (bit 15 clear)' : ''}`);
    }
    return parts.join(', ');
}
const md = (s) => String(s).replace(/\|/g, '\\|');

// Hand-written prose. Keep policies in sync with docs/DESIGN.md sections 3 and 6.
const PROSE = {
    intro: `This is the reference of the realtime protocol spoken between the Scacelith game client and
a Scacelith dedicated server over a WebSocket. The tables are generated from
\`src/protocol/schema.js\` by \`tools/gen-protocol.js\` (\`npm run gen:protocol\`); the prose is kept in
the generator. The HTTPS account API is described in \`docs/DESIGN.md\` section 5.9.`,

    wire: `* One protocol message per WebSocket **binary** message. A text message is a protocol violation.
  Frames are never compressed (no extension is negotiated). The server refuses a client message
  larger than \`WS_MAX_MESSAGE_BYTES\` (default 512) from the frame header, before buffering it.
* A message is a type byte followed by its fields in schema order, **little-endian, no padding**:

  \`\`\`
  u8 type | field 1 | field 2 | ...
  \`\`\`

  Type bytes \`0x01\`-\`0x7F\` are client->server (C2S), \`0x80\`-\`0xFF\` server->client (S2C). The
  names used by both directions (currently \`Ping\`, \`Pong\` and \`Gesture\`) have a different id
  in each; code names them \`C_Ping\`/\`S_Ping\`, \`C_Pong\`/\`S_Pong\`, \`C_Gesture\`/\`S_Gesture\`.
* Every C2S message starts with \`seq\` (u32): 1 for \`Hello\`, then +1 for each message sent on the
  connection (Pongs included). Replies that refer to a request quote it as \`ref\`.
* A message must be consumed exactly. The decoder rejects, and the server treats as malformed:
  truncation, trailing bytes, an unknown type byte, a type of the wrong direction, an integer
  outside its bounds, an enum value that is not in the enum, a bool other than 0/1, a non-finite
  f64, an id53 >= 2^53, a string that is not valid UTF-8 (strict: no overlong forms, no encoded
  surrogates; a leading BOM is kept as U+FEFF), that contains a NUL byte or whose length is outside
  its bounds, a list longer than its maximum.
* The encoders validate the same rules, so a server bug cannot produce a frame the client refuses.

| Type | Wire | Notes |
|---|---|---|
| \`u8\` \`u16\` \`u32\` \`i32\` | 1, 2, 4, 4 bytes | integers; optional schema bounds (\`min\`, \`max\`) are enforced |
| \`f64\` | 8 bytes | IEEE-754 double; wall-clock times are milliseconds since the Unix epoch |
| \`id53\` | 8 bytes (u64) | game id, value < 2^53 (exact in a JS number and a C++ \`uint64_t\`); 0 = none |
| \`bool\` | 1 byte | 0 or 1 |
| \`str8\` | u8 length + bytes | UTF-8, no NUL, at most 255 bytes (tighter bounds per field) |
| \`enum:Name\` | 1 byte (u8) | one of the enum's values (all enum values are 0..255) |
| \`struct:Name\` | the struct's fields inline | |
| \`list16:T\` | u16 count + items | items of type T; the maximum count is per field |`,

    versioning: `* \`PROTOCOL_VERSION\` is the version a build speaks; the server accepts any \`Hello.proto\` in
  \`[PROTOCOL_MIN, PROTOCOL_VERSION]\`. Every change of the wire format bumps \`PROTOCOL_VERSION\`;
  \`PROTOCOL_MIN\` is raised only when the server drops an old version.
* \`SCHEMA_HASH\` (u32) is the first 4 bytes (big-endian) of SHA-256 of the canonical JSON of the schema
  (\`{version, enums, structs, messages}\`, keys sorted, \`doc\` fields excluded), computed by
  \`computeSchemaHash()\`, shared by the JS and the C++ generators. The client sends it in
  \`Hello.schema\`. It catches a client and a server generated from different schema files even when
  \`PROTOCOL_VERSION\` is the same (a forgotten bump).
* The WebSocket subprotocol is \`${defaultSchema.WS_SUBPROTOCOL}\` (\`Sec-WebSocket-Protocol\`); its number follows
  major, incompatible generations of the protocol. A server that does not select it is not a
  Scacelith server of this generation: the client stops there.
* \`GET /api/v1/info\` announces \`protocol: {min, max, schema, subprotocol}\` before any connection, so
  the client can tell the player before trying (\`ServerInfo.compatible\` in the game).
* On a mismatch the server answers \`Error{UnsupportedProtocol, fatal}\` and closes with 4002. The
  client does **not** retry automatically: it shows "This server runs a different version of
  Scacelith: update the game (or ask the server administrator to update the server)", with both
  versions, and stays in the \`Incompatible\` state until the player changes server or updates.
* Compatible evolution inside one version is not attempted: there are no optional fields and no
  unknown-field skipping. Adding a message or a field, or changing a bound or an enum, changes
  \`SCHEMA_HASH\` and requires a new \`PROTOCOL_VERSION\`.`,

    lifecycle: `1. **Connect.** \`wss://host:WS_PORT/ws\` (the port and path come from \`/api/v1/info\`), with
   \`Sec-WebSocket-Protocol: ${defaultSchema.WS_SUBPROTOCOL}\` and no \`Origin\` header (browsers are refused unless the
   server allows their origin). TLS certificate validation follows the server's trust settings
   (OS store or a pinned certificate for self-signed community servers). The \`101\` answer carries
   \`Scacelith-Server-Id\`, the \`serverId\` of \`/api/v1/info\`: a client whose saved session at that
   origin belongs to another server id drops it instead of sending it in \`Hello\`.
2. **Hello.** Within \`WS_HELLO_TIMEOUT_MS\` (default 10 s; close 4010 otherwise) the client sends
   \`Hello{seq: 1, proto, schema, client, token}\` where \`token\` is the session token obtained from
   *this* server's HTTPS login. Any other message before \`Hello\` is \`Error{HelloRequired, fatal}\`.
3. **Welcome.** The server validates the token (\`Unauthorized\`, close 4003), bans (\`Banned\`, 4004),
   e-mail verification (\`EmailUnverified\`), capacity (\`ServerFull\`, close 4006, for a new player
   beyond \`MAX_CONNECTIONS\`; a player whose game is in progress is admitted), claims the account's
   presence (an older connection of the same account receives \`Error{Replaced, fatal}\` + close
   4007) and answers \`Welcome{proto, serverTime, userId, username, serverName, heartbeatMs,
   clientPingMs, maxMsgPerSec, activeGame, gestureRate, gestureBurst}\`. \`serverTime\` gives the
   client a first estimate of the server clock offset (it is read once the session is accepted, on
   the clock of the game hosts: [Clocks](#clocks)); \`clientPingMs\` is the interval of the client's
   own \`Ping\` (below); \`gestureRate\` and \`gestureBurst\` announce the
   [gesture relay](#gesture-relay) (0 and 0: no relay, send no \`Gesture\`).
4. **Game in progress.** When \`activeGame != 0\` the host of that game sends a \`GameSnapshot\` right
   after \`Welcome\`; the client rebuilds the board and the clocks from it.
5. **Heartbeats.** The server sends \`Ping{nonce, serverTime}\` about every \`heartbeatMs\`
   (\`HEARTBEAT_INTERVAL_MS\`, default 10 s: two pings are between half of it and all of it apart,
   plus the server's sweep period of 250 ms); the client answers \`Pong{seq, nonce}\` **at once** (the
   server measures each player's round trip with it and uses it for lag compensation). A connection
   silent for \`HEARTBEAT_TIMEOUT_MS\` (default 30 s) is closed. The client measures its own round
   trip and clock offset with \`Ping{seq, nonce}\` (at most one per second), answered by
   \`Pong{nonce, serverTime}\`: \`offset = serverTime - (sentAt + rtt / 2)\`. It sends one at once
   after \`Welcome\` and three more about a second apart (so the ping indicator and the clock offset
   are right quickly), then one every \`Welcome.clientPingMs\` (\`CLIENT_PING_INTERVAL_MS\`, default
   10 s, 1 s to 60 s; 0 means the client's default of 10 s). Each of these pings costs server CPU
   for every connected player, so the server chooses the interval. The client decides that the
   connection is dead when nothing at all was received for twice \`heartbeatMs\` (10 s at least).
   After 1.5 times \`heartbeatMs\` (7.5 s at least) without anything, it sends one \`Ping\` of its own
   at once: its \`Pong\` keeps a live connection when a heartbeat comes late.
6. **Reconnection.** A lost connection does not stop a game: the player's clock keeps running (except
   in a game restored after a restart, below) and the opponent receives
   \`GameEvent{PlayerDisconnected, arg = grace ms}\`. The client reconnects with
   exponential backoff and full jitter (attempt n waits a uniform random time between 0.5 s and
   min(30 s, 2 s x 2^n); n is reset by a successful \`Welcome\`), sends a new \`Hello\` (seq starts
   again at 1 on the new connection) and receives \`Welcome\` then \`GameSnapshot\`. A full server
   (HTTP 503 at the upgrade, \`Error{ServerFull}\` or close 4006) is retried after 60 s to 120 s.
   After a shutdown (\`Notice{ServerShutdown}\`, \`Error{ShuttingDown}\` or close 4008) the first
   attempt waits 5 s to 35 s, which spreads the reconnection wave of a restart, and the first HTTP
   503 that follows is the restart, retried like a failure (a later one is a full server again).
   A player whose game is in progress only has the reconnection grace to come back: at least
   \`RECONNECT_GRACE_MIN_MS\` (15 s by default), and \`RECOVERY_GRACE_MS\` (90 s by default) for a
   game the server restored after a restart. Their attempts are 8 s apart at most, whatever the
   cause, unless the server gave a \`Retry-After\`, and their first attempt after a shutdown waits
   1 s to 8 s. In a game the server restored after a restart, the clock of the side to move (its
   first-move timer at plies 0 and 1) stays stopped until that player is back, for
   \`RECOVERY_CLOCK_HOLD_MS\` at most (20 s by default, never more than the recovery grace). While
   it is held, the game's \`GameSnapshot\` has \`running = None\` even from ply 2 on (where a clock
   normally runs), and at plies 0 and 1 its \`firstMoveMs\` includes the rest of the hold. When
   the held clock starts (its player is back, or the hold is over while they are still away), the
   opponent receives a \`GameSnapshot\` it did not ask for, with that clock running; the player who
   is back receives theirs after \`Welcome\`, as after any reconnection. A player back within the
   hold loses no clock time to the restart; after it, their clock runs whether they are back or
   not. For 10 minutes after losing a connection that had reached \`Welcome\`, after a shutdown as
   after a network failure, the automatic attempts reuse the \`/api/v1/info\` answer (\`wsPath\`)
   that connection was made with instead of asking for it again (one TLS handshake instead of two,
   so the reconnection wave of a restart costs one per player). What a restart can change is still
   caught: another server at that origin by the server id of the \`101\` answer (step 1), before
   \`Hello\`; another protocol at \`Hello\` (close 4002). A 5xx at the upgrade keeps that answer (a
   reverse proxy answers 502 while the server restarts); a 4xx other than 429 (404 for another
   path, 426 for another subprotocol) makes the next attempt read it again. A connection the
   player asks for always reads it first. The client never replays move intents from the old
   connection blindly: the snapshot says which moves the server accepted. It does not reconnect
   after \`Replaced\`, \`Banned\`, \`Unauthorized\`, \`UnsupportedProtocol\` or \`CheatDetected\`.
7. **Closing.** The server closes with the codes below; a fatal \`Error\` precedes the close when there
   is one. \`Notice{ServerShutdown, arg = ms}\` announces a restart: the client reconnects afterwards
   (games in progress are replayed from the server's journal, and their players then have
   \`RECOVERY_GRACE_MS\`, 90 s by default, to come back, with the clock hold of step 6).`,

    gameflow: `* **Intents, not commands.** Every C2S game message is a request; the server decides. A client never
  shows a move, a result or a clock value that the server did not confirm, except for the local
  animation of its own move.
* **Start.** A game starts with \`GameSnapshot\` (sent to both players when the matchmaker, a challenge
  or a rematch creates it). \`you\` is the receiver's colour, \`firstMoveMs\` the time left for the next
  player's first move. \`autoPress\` says who presses the clock in this game (below): the server
  decides it when it creates the game (\`AUTO_PRESS_CLOCK\`, true by default; a rematch keeps the
  value of the game it follows) and the game keeps it to the end, across a server restart too.
* **Moves.** The client validates the move with the same rules as the server, then sends
  \`Move{seq, game, ply, move, posHash, thinkMs, drawOffer}\`. With \`autoPress\` it sends it at once,
  when the destination square is chosen (the robot's hand then plays the move and presses the clock:
  animation only). Without it, the move placed on the board is not sent until the player presses
  the clock (\`Gesture.placed\` shows it to the opponent meanwhile), so the clock runs until the press,
  as over the board, and \`thinkMs\` runs until the press too. \`ply\` is the index of the move in
  the game (0 = White's first move), \`posHash\` the digest of the position the move is played in,
  \`thinkMs\` the locally measured time since the turn began.
* **Confirmation.** The server validates (order below), updates the position and the clocks and
  sends one \`MoveMade\` to **both** players. For the mover it is the confirmation; for the opponent it
  is the move to animate. \`gseq\` numbers the game's events (MoveMade, GameEvent, GameEnd) so that a
  client can discard duplicates after a resync.
* **Rejection.** \`MoveRejected{game, ply, move, code}\` goes to the mover only (the opponent never
  sees a rejected move). When the client must resynchronise (\`Desync\`, \`StalePly\` with a different
  move, \`NotYourTurn\`...) a \`GameSnapshot\` follows; the client restores the position and the clocks
  from it. \`FlagFell\` is followed by \`GameEnd\`.
* **Validation order** of a Move (DESIGN.md 6.2): not a participant -> \`NotInGame\`; game over ->
  \`GameOver\`; \`ply\` already played -> the original \`MoveMade\` again if the move is identical,
  else \`StalePly\`; \`posHash\` differs from the server position -> \`Desync\` + \`GameSnapshot\`;
  \`ply\` ahead -> \`Desync\`; not the sender's turn -> \`NotYourTurn\`; illegal -> \`IllegalMove\`;
  clock check (flag) -> \`FlagFell\`; then the move is played. A pending draw offer from the opponent
  is declined by the move.
* **Draws.** \`DrawOffer\` (or \`Move.drawOffer\`) -> the opponent receives \`GameEvent{DrawOffered}\` and
  answers with \`DrawAnswer\`; a move instead of an answer declines. At most \`DRAW_OFFERS_PER_GAME\`
  per player and none within 10 plies after a decline (\`DrawOfferLimit\`). \`DrawClaim\` succeeds on a
  threefold repetition or at 100 halfmoves, else \`NothingToClaim\`. Mate, stalemate, insufficient
  material, fivefold repetition and the 75-move rule end the game automatically.
* **Resign / abort.** \`Resign\` at any time while the game runs (leaving a running game from the menu
  is a resignation). \`Abort\` only before one's own first move (\`AbortNotAllowed\` otherwise).
* **End.** \`GameEnd{status, reason, whiteMs, blackMs, serverTime}\` to both players. For a rated game
  \`RatingUpdate\` follows once the result is committed to the database. \`Rematch{accept}\` within
  60 s: when both accept, the server creates the new game (colours swapped) and sends its
  \`GameSnapshot\`; it expires when a player leaves.
* **Disconnection.** The opponent receives \`GameEvent{PlayerDisconnected, arg = grace}\` then
  \`PlayerReconnected\`; grace = clamp(base / 10, \`RECONNECT_GRACE_MIN_MS\`, \`RECONNECT_GRACE_MAX_MS\`).
  After the grace the absent player loses by \`Abandonment\` (or the game is aborted before the
  second ply).`,

    clocks: `* The server is the only authority on time. The client displays the clocks from the last
  \`GameSnapshot\` / \`MoveMade\` / \`GameEnd\` and its estimate of the server clock
  (\`serverNow = localNow + offset\`, offset from \`Welcome.serverTime\` and the Ping/Pong exchanges):
  \`shown(running side) = xMs - (serverNow - serverTime)\`; the other side's clock is \`xMs\` as sent.
* \`whiteMs\` / \`blackMs\` are the remaining times **at \`serverTime\`** (server clock, epoch ms). Every
  \`serverTime\` (\`Welcome\`, \`Ping\`, \`Pong\`, \`GameSnapshot\`, \`MoveMade\`, \`GameEnd\`) is read from
  the monotonic epoch clock of a server worker, so the offset measured with one applies to the
  others (to a few milliseconds when the connection and the game are on different workers).
  \`running\` is the colour whose clock is running from that instant (\`None\` before the clocks start,
  after the end, while waiting for a first move, or while the clock of a game restored after a
  restart waits for its player, \`RECOVERY_CLOCK_HOLD_MS\` at most: lifecycle step 6). In
  \`MoveMade\` the clock of the side to move runs from \`serverTime\` unless \`firstMoveMs > 0\`.
* Plies 0 and 1 (each side's first move) do not run the clock: each player has \`firstMoveMs\`
  (\`FIRST_MOVE_TIMEOUT_MS\`) to make it, otherwise the game is aborted (\`NoShow\`, unrated). The
  server accepts a first move that arrives within that time plus the margin a flag has (the
  player's largest lag compensation, below), so that a first move sent in time over a slow link
  counts; \`firstMoveMs\`, the countdown shown, has no margin. No increment for them. The clocks
  start with White's second move.
* For every later move the server charges \`elapsed - compensation\`, where \`elapsed\` runs from the
  moment it sent the opponent's \`MoveMade\` to the moment it received the move, and the lag
  compensation is bounded by the client's \`thinkMs\`, the server-measured round trip (+50 ms),
  \`LAG_COMP_MAX_MS\` and a per-player quota (DESIGN.md 6.1). \`thinkMs\` never adds time; an impossible
  one (\`thinkMs > elapsed + 100\`) is recorded as an anomaly. The increment is added after the move;
  \`MoveMade.spentMs\` is the time charged, \`MoveRec.clockMs\` the mover's remaining time after the
  increment.
* A flag falls on the server (timer at the latest instant a move could still arrive in time):
  \`GameEnd{Timeout}\` (or \`TimeoutVsInsufficient\`, a draw, when the opponent cannot mate). A move
  arriving after that is refused with \`FlagFell\`. The client never ends a game on its own clock.
* **Server stalls.** When the server process hosting a game stops for a moment (more than
  \`GAME_STALL_MIN_MS\`, 30 ms by default: garbage collection, disk I/O, CPU steal, or reading the
  backlog of such a stop), what waited in its sockets meanwhile is handled before its timers, as if
  it had arrived when the stall began (at most \`GAME_STALL_CREDIT_MAX_MS\`, 5 s, before it was
  read): a move, a resignation, a draw agreement or claim, an abort, a \`Resync\` or a closed
  connection is never overtaken by a flag or a first-move timeout that fell during the stall.
  Such a move is charged the time until the stall began, and the next player's clock starts when
  its \`MoveMade\` is sent, so the stall is charged to nobody. A first-move timeout that fell during
  a stall still aborts the game, without counting a no-show against the player, and a \`Pong\`
  whose \`Ping\` preceded a stall is left out of the round-trip average.`,

    gestures: `* **What.** \`Gesture\` carries a player's live, cosmetic state to the opponent, whose robot mirrors
  it: the head (\`yaw\` and \`pitch\` of the look in milliradians, seat-relative: 0 is straight ahead
  and level, \`yaw\` > 0 to the left and \`pitch\` < 0 down; \`lean\` towards the board, in percent;
  \`GestureFlag\` tells a glance and a look at the table beside the board, the scoresheet included),
  the piece in hand (\`touch\`) and where it is aimed (\`aim\`), and the move placed on the board before
  the clock press (\`placed\`, in games without \`autoPress\`). It is never authoritative: only \`Move\`
  plays a move, and the server does not look inside a gesture beyond checking that it decodes.
* **Sending.** The client sends \`Gesture{seq, game, ply, ...}\` for its game in progress when its
  state changes, and at least once a second even when nothing changed, for the whole game and on the
  opponent's turn too (the reference client stops following the opponent's head 2.5 s after its last
  gesture and puts back a piece it mirrors after 5 s). It sends the whole state every time (a lost
  gesture heals with the next one), at most \`Welcome.gestureRate\` per second sustained with bursts
  of \`Welcome.gestureBurst\` (\`GESTURE_RATE\`, 4, and \`GESTURE_BURST\`, 8, by default), so every player
  in a game sends between one and \`gestureRate\` gestures per second. When \`gestureRate\` is 0 the
  server relays nothing and the client sends none. A gesture takes the next \`seq\` like any message.
* **ply.** The number of plies played when the current state of the hand (\`touch\`, \`aim\`, \`placed\`,
  \`Promoting\`) began: while a move is being prepared, the ply of that move. A change of the head
  alone keeps it, so an idle hand keeps the ply at which it went idle, through the opponent's moves.
* **Relay.** The server forwards it to the opponent's connection as the server \`Gesture\`: the client
  message without \`seq\`, byte for byte. It never goes back to the sender, and it is never stored
  (not in the game's journal, not in the finished game) nor seen by the game's clocks, \`gseq\` or the
  anti-cheat. It is relayed while the game exists, the rematch window included.
* **Receiving.** The latest gesture wins. Its head (\`yaw\`, \`pitch\`, \`lean\`, \`Glance\`, \`Side\`) always
  applies, whatever its \`ply\`. Its hand (\`touch\`, \`aim\`, \`placed\`, \`Promoting\`) describes a move
  being prepared and applies only while the receiver's game has exactly \`ply\` plies and it is the
  sender's turn: a gesture of an earlier ply is stale (that move is known), and one of a later ply
  is ahead of a \`MoveMade\` the receiver has not got yet (a gesture and a \`MoveMade\` may cross when
  the players are on different server workers). The reference client also checks the hand against
  its own position (the sender's pieces, legal destinations and moves only): gestures come from the
  other client and are not trusted.
* **Silent drops.** Gestures have a token bucket of their own: they never spend or wait for the
  \`WS_MSG_RATE\` tokens of moves and requests, and a gesture beyond \`gestureRate\` is dropped with no
  \`Error{RateLimited}\` (its \`seq\` still counts, so the next message is in sequence). Only a gross
  excess closes the connection as a flood (\`Error{Flood, fatal}\`, 4301): more than max(50, 10 x
  \`gestureBurst\`, \`gestureRate\` x (the server's heartbeat timeout + \`heartbeatMs\` + 0.25 s)) dropped
  in 10 s, 161 by default. The server counts gestures as it reads them: after a stall of the network
  or of the server, the gestures that a client pacing them at \`gestureRate\` sent meanwhile arrive
  together and all but a burst of them are dropped, but that never adds up to a flood while the
  connection lives. A gesture for a game the connection does not play, for an opponent who is not
  connected, or towards a connection or a link between the server's workers that already holds a
  backlog of unsent data (a quarter of its limit) is dropped as well: a slow link loses gestures
  first, and a gesture never closes a slow client. A gesture that does not decode is a protocol
  violation like any message (4300).`,

    replay: `* **seq.** Each C2S message carries \`seq\` = previous + 1 (Hello = 1) on its connection. The server
  drops a message whose \`seq\` is not exactly last + 1 (anomaly \`bad_seq\`), so a replayed or
  reordered frame on a connection has no effect. A new connection starts again at 1 and needs a
  fresh \`Hello\` with a valid token.
* **ply.** A move names the ply it is for. A move for a ply already played is a duplicate: when it is
  the same move the server re-sends the original \`MoveMade\` (idempotent: a client that lost the
  confirmation during a reconnection may resend it safely); a different move gets \`StalePly\`.
* **posHash.** The move is only valid in the position it was chosen in; a stale view is detected
  (\`Desync\`) instead of playing the move in another position.
* **gseq.** S2C game events carry the game's event number; a client ignores an event whose \`gseq\` is
  not newer than the snapshot it has.
* **Tokens.** \`Hello.token\` is a session token of this server only (credentials are scoped per
  server origin); it can be revoked at any time (\`Notice{SessionRevoked}\` then close).`,

    ratelimits: `* Per connection: a token bucket of \`WS_MSG_RATE\` messages per second (default 20) with a burst of
  \`WS_MSG_BURST\` (default 40); \`Welcome.maxMsgPerSec\` tells the client the sustained rate. An
  exceeded bucket answers \`Error{RateLimited}\`; repeated excess is \`Error{Flood, fatal}\` + close 4301.
* Client \`Ping\`: at most one per second (a faster one gets no \`Pong\`); the interval the server wants
  is \`Welcome.clientPingMs\`.
* \`Gesture\`: a bucket of its own (\`Welcome.gestureRate\` / \`gestureBurst\`) that never touches the one
  above; beyond it gestures are dropped silently ([Gesture relay](#gesture-relay)).
* Per IP address (an IPv4 address or an IPv6 /64): \`MAX_CONNECTIONS_PER_IP\` simultaneous WebSocket
  connections, counted exactly over the workers (HTTP 429 \`too_many_connections\` at the upgrade
  beyond it; this limit has no count per IPv6 /48). The upgrade request also takes a token of the
  address's request budget like any HTTP request (\`HTTP_RATE_PER_IP\`, and for IPv6 also
  \`HTTP_RATE_PER_PREFIX\` for its /48 as a whole), and an address that keeps going after being
  refused is blocked for a while: either way the upgrade gets HTTP 429 with \`Retry-After\` and
  \`{ "error": "rate_limited", "message": "...", "retryAfter": s }\`, and the client waits that long.
  With native TLS, a new connection from a blocked address, or beyond \`IP_CONN_RATE\` new
  connections per second or \`IP_MAX_CONNECTIONS\` open ones from its address (an IPv6 /48 as a
  whole: 4 times each), is reset before the TLS handshake (a connection failure for the client,
  retried with its backoff).
  A block never closes a WebSocket connection already open (players who share an address with an
  abuser keep their games). See the README, "Protection against abuse". Whole server:
  \`MAX_CONNECTIONS\` players (\`Error{ServerFull}\` at \`Hello\` beyond it, except for a player whose
  game is in progress). The upgrade itself is refused (HTTP 503) only \`max(16, 2 %)\` connections
  beyond it, so that such a player can reach \`Hello\`.
* A client that does not read its messages (more than \`WS_SEND_BUFFER_LIMIT\` bytes queued) is closed
  with 4303 (\`SlowConsumer\`); it reconnects and resynchronises from the snapshot.
* Challenges, private games and queue joins have their own limits (\`ChallengeLimit\`, also after 5
  direct challenges withdrawn or declined in a minute; \`RateLimited\` for \`ChallengeJoinCode\` after
  10 wrong codes in a minute; \`MatchmakingCooldown\` with \`Notice{MatchmakingCooldown, arg = until}\`).`,

    errors: `* \`Error{ref, code, fatal, game}\`: \`ref\` is the \`seq\` of the refused request (0 when none), \`game\`
  the game concerned (0 when none). \`fatal\` = the server closes the connection right after it, with
  the matching close code below: \`4000 + code\` for the connection errors (e.g. \`Unauthorized\` ->
  4003; \`EmailUnverified\` closes with 4003 too), \`4300\`-\`4303\` for \`ProtocolViolation\`, \`Flood\`,
  \`CheatDetected\` and \`SlowConsumer\`.
* Requests without another answer are confirmed with \`Ack{ref}\` (queue leave, challenge decline or
  cancel, draw offer...); requests with an answer get that answer (\`QueueStatus\`,
  \`ChallengeStatus\`, \`MoveMade\`, \`GameSnapshot\`...).
* A frame the server cannot decode after \`Hello\` is a protocol violation: \`Error{Malformed, fatal}\`
  or \`Error{ProtocolViolation, fatal}\` and close 4300. A C2S type byte from the S2C range is a
  certain protocol forgery (\`CheatDetected\`, 4302).
* Game errors (\`NotInGame\` ... \`FlagFell\`) come as \`MoveRejected\` for moves and as \`Error\` with the
  \`game\` field for the other game requests. They are never fatal by themselves (except when the
  anti-cheat sanctions a certain cheat: \`Error{CheatDetected, fatal}\`, close 4302).
* A client that cannot decode a server message closes with 1002 and reconnects; it reports the
  incident in its log (it means a version or schema mismatch that the Hello check did not catch).`,

    moves: `A move is a u16: \`from | to << 6 | promo << 12\`. Squares are 0..63 with a1 = 0, b1 = 1, ..., h8 = 63
(\`square = file + 8 * rank\`). \`promo\` uses the chess::PieceType numbering: 0 none, 2 knight,
3 bishop, 4 rook, 5 queen. Castling is the king's two-square move (e1g1, e1c1, e8g8, e8c8). Bit 15
must be 0 (\`move <= 0x7FFF\` in C2S messages and in \`MoveRec\`). \`MoveMade.flags\` is a set of
\`MoveFlag\` bits describing the move as played (capture, en passant, castling, promotion, check,
mate). Helpers: \`encodeMove(from, to, promo)\`, \`decodeMove(u16)\`, \`moveToUci\`, \`uciToMove\` in
\`src/protocol/index.js\`; \`net::packMove\` in the game.

Examples: e2e4 = 12 | 28 << 6 = \`0x070C\` (1804); e7e8=Q = 52 | 60 << 6 | 5 << 12 = \`0x5F34\`;
White O-O = e1g1 = 4 | 6 << 6 = \`0x0184\`.`,

    poshash: `\`posHash\` (u32) is the FNV-1a 32-bit hash of the ASCII text made of the first four FEN fields
\`placement side castling ep\` separated by single spaces, exactly the prefix of
\`chess::Position::fen()\` in the game: castling in \`KQkq\` order or \`-\`, and the en passant square
written **only when an en passant capture is actually legal** (\`-\` otherwise). FNV-1a 32: start with
\`0x811C9DC5\`, for each byte \`h = (h XOR byte) * 0x01000193 mod 2^32\`. Helpers: \`fnv1a32(text)\` and
\`fenDigest(fen)\` in \`src/protocol/index.js\`, \`Position.digest()\` in the server's chess module,
\`net::positionDigest(fen)\` in the game.

The start position digests
\`rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -\` = \`${'${START_DIGEST}'}\`.`,

    performance: `The generated codec is straight-line code specialised per message (fixed offsets, one bounds check
per run of fixed-size fields, exact-size allocation, ASCII fast paths for strings). Measured with
\`node tools/gen-protocol.js --bench\` (Node 22.22, one core of the development container; encode +
decode of one message, operations per second, higher is better):

${'${BENCH_TABLE}'}

Decoding errors carry no stack trace (it would cost more than the decoding itself); encoding errors
keep theirs (they are server bugs).`,
};

// Benchmark results recorded in the documentation (update after codec changes with --bench).
const BENCH_TABLE = `| Message | interpreted (placeholder) | generated | speed-up | generated encode only | generated decode only |
|---|---:|---:|---:|---:|---:|
| \`Move\` (C2S, 26 bytes) | 1.38 M/s | 8.91 M/s | 6.5x | 16.22 M/s | 28.21 M/s |
| \`MoveMade\` (S2C, 43 bytes) | 0.97 M/s | 6.75 M/s | 6.9x | 10.39 M/s | 20.36 M/s |
| \`GameSnapshot\` with 80 moves (S2C, 895 bytes) | 25.0 k/s | 326.5 k/s | 13.1x | 653.9 k/s | 711.2 k/s |

Runs vary by about 10 %; a second run gave 6.5x, 6.0x and 16.0x.`;

/**
 * Generates docs/PROTOCOL.md.
 * @param {object} [s] schema module
 * @returns {string}
 */
export function generateDocs(s = defaultSchema) {
    const model = buildModel(s);
    const hash = computeSchemaHash(s);
    const hashHex = '0x' + hash.toString(16).toUpperCase().padStart(8, '0');
    const L = [];
    const p = (x = '') => L.push(x);
    p('# Scacelith realtime protocol');
    p();
    p('<!-- GENERATED by tools/gen-protocol.js from src/protocol/schema.js: do not edit, run `npm run gen:protocol`. -->');
    p();
    p(PROSE.intro);
    p();
    p('| | |');
    p('|---|---|');
    p(`| \`PROTOCOL_VERSION\` | ${s.PROTOCOL_VERSION} |`);
    p(`| \`PROTOCOL_MIN\` | ${s.PROTOCOL_MIN} |`);
    p(`| \`SCHEMA_HASH\` | \`${hashHex}\` (${hash}) |`);
    p(`| WebSocket subprotocol | \`${s.WS_SUBPROTOCOL}\` |`);
    p(`| Messages | ${model.messages.filter((m) => m.dir === 'c2s').length} client->server, ${model.messages.filter((m) => m.dir === 's2c').length} server->client |`);
    p();
    p('Contents: [Wire format](#wire-format) · [Versioning](#versioning-and-compatibility) · [Connection](#connection-lifecycle) ·');
    p('[Game flow](#game-flow) · [Clocks](#clocks) · [Gestures](#gesture-relay) · [Replay protection](#duplicates-and-replay-protection) ·');
    p('[Rate limits](#rate-limits) · [Errors](#error-handling) · [Messages](#messages) · [Structs](#structs) ·');
    p('[Enums](#enums) · [Moves](#move-encoding) · [posHash](#position-digest-poshash) · [Close codes](#websocket-close-codes) ·');
    p('[Codec performance](#codec-performance)');
    p();
    p('## Wire format');
    p();
    p(PROSE.wire);
    p();
    p('## Versioning and compatibility');
    p();
    p(PROSE.versioning);
    p();
    p('## Connection lifecycle');
    p();
    p(PROSE.lifecycle);
    p();
    p('## Game flow');
    p();
    p(PROSE.gameflow);
    p();
    p('## Clocks');
    p();
    p(PROSE.clocks);
    p();
    p('## Gesture relay');
    p();
    p(PROSE.gestures);
    p();
    p('## Duplicates and replay protection');
    p();
    p(PROSE.replay);
    p();
    p('## Rate limits');
    p();
    p(PROSE.ratelimits);
    p();
    p('## Error handling');
    p();
    p(PROSE.errors);
    p();
    p('## Messages');
    p();
    p('| Id | Name | Direction | Size (bytes) |');
    p('|---|---|---|---|');
    for (const m of model.messages) {
        const size = m.size >= 0 ? String(m.size) : `${1 + m.fields.reduce((a, f) => a + minSize(f.t), 0)}+`;
        p(`| \`${hex(m.id)}\` | [${m.key}](#${m.key.toLowerCase()}) | ${m.dir === 'c2s' ? 'client -> server' : 'server -> client'} | ${size} |`);
    }
    p();
    for (const m of model.messages) {
        p(`### ${m.key}`);
        p();
        p(`\`${hex(m.id)}\`, ${m.dir === 'c2s' ? 'client -> server' : 'server -> client'}, ${m.size >= 0 ? `${m.size} bytes` : `at least ${1 + m.fields.reduce((a, f) => a + minSize(f.t), 0)} bytes`}.${m.doc ? ' ' + m.doc : ''}`);
        p();
        p('| Field | Type | Bytes | Limits |');
        p('|---|---|---|---|');
        for (const f of m.fields) p(`| \`${f.name}\` | ${md(typeLabel(f.t))} | ${sizeLabel(f.t)} | ${md(limitLabel(f.t, f.opts))} |`);
        p();
    }
    p('## Structs');
    p();
    const fieldDocs = structComments();
    for (const st of model.structs.values()) {
        p(`### ${st.name}`);
        p();
        p(st.size >= 0 ? `${st.size} bytes.` : `At least ${minSize(st)} bytes.`);
        p();
        p('| Field | Type | Bytes | Limits | Notes |');
        p('|---|---|---|---|---|');
        for (const f of st.fields) {
            p(`| \`${f.name}\` | ${md(typeLabel(f.t))} | ${sizeLabel(f.t)} | ${md(limitLabel(f.t, f.opts))} | ${md(fieldDocs[`${st.name}.${f.name}`] || '')} |`);
        }
        p();
    }
    p('## Enums');
    p();
    p('Every enum travels as a u8; a value outside the enum is malformed.');
    p();
    const enumDocs = enumComments();
    for (const e of Object.values(model.enums)) {
        p(`### ${e.name}`);
        p();
        p('| Value | Name | Notes |');
        p('|---|---|---|');
        for (const [k, v] of e.entries) p(`| ${v} | \`${k}\` | ${md(enumDocs[`${e.name}.${k}`] || '')} |`);
        p();
    }
    p('### MoveFlag (bit set, `MoveMade.flags`)');
    p();
    p('| Bit | Name |');
    p('|---|---|');
    for (const [k, v] of Object.entries(s.MoveFlag)) p(`| \`${hex(v)}\` | \`${k}\` |`);
    p();
    p('### GestureFlag (bit set, `Gesture.flags`)');
    p();
    p('| Bit | Name |');
    p('|---|---|');
    for (const [k, v] of Object.entries(s.GestureFlag)) p(`| \`${hex(v)}\` | \`${k}\` |`);
    p();
    p('## Move encoding');
    p();
    p(PROSE.moves);
    p();
    p('## Position digest (posHash)');
    p();
    p(PROSE.poshash.replace('${START_DIGEST}', '0x' + fnv('rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -').toString(16).toUpperCase().padStart(8, '0')));
    p();
    p('## WebSocket close codes');
    p();
    p('| Code | Name | Meaning |');
    p('|---|---|---|');
    for (const [k, v] of Object.entries(s.CloseCode)) p(`| ${v} | \`${k}\` | ${CLOSE_MEANING[k] || ''} |`);
    p();
    p('## Codec performance');
    p();
    p(PROSE.performance.replace('${BENCH_TABLE}', BENCH_TABLE));
    p();
    p('## Golden vectors');
    p();
    p('`test/fixtures/protocol-vectors.json` (generated by `tools/gen-protocol-vectors.js`) lists, for every');
    p('message, field values and their exact encoding (`hex`), plus malformed inputs that every decoder must');
    p('refuse (`malformed[]`: `hex`, `dir` = direction the bytes are received in, `reason` given by the JS');
    p('codec). The JS tests and the C++ client tests (`tests/net_tests.cpp`) both check them.');
    p();
    return L.join('\n');
}

const CLOSE_MEANING = {
    Normal: 'normal closure (logout, client quit)',
    GoingAway: 'server restart or client shutting down',
    ProtocolError: 'WebSocket-level protocol error',
    Unsupported: 'text frame or unsupported data',
    Policy: 'refused by policy (e.g. an Origin not allowed)',
    TooBig: 'message larger than `WS_MAX_MESSAGE_BYTES`',
    Internal: 'unexpected server error',
    UnsupportedProtocol: '`Hello.proto` / `Hello.schema` not supported: update the game or the server (no automatic retry)',
    Unauthorized: 'session token refused (log in again) or e-mail address not verified (`Error{EmailUnverified}`): no automatic retry',
    Banned: 'account banned (a `Notice{Banned}` gives the end)',
    ServerFull: 'a new player beyond `MAX_CONNECTIONS` (a player whose game is in progress is admitted): retried after 60 s to 120 s',
    Replaced: 'another connection of the same account took over (no automatic retry)',
    ShuttingDown: 'server shutting down: reconnect later',
    HelloTimeout: 'no `Hello` within `WS_HELLO_TIMEOUT_MS`',
    ProtocolViolation: 'undecodable frame, text frame or seq violation after Hello',
    Flood: 'rate limit exceeded repeatedly',
    CheatDetected: 'certain protocol cheat (forged type, illegal move, move out of turn)',
    SlowConsumer: 'client does not read its messages',
};

// Inline comments of the enum values in schema.js (documentation only; always the file on disk,
// whatever schema generateDocs is given).
function enumComments() {
    const out = {};
    let src = '';
    try { src = fs.readFileSync(path.join(ROOT, 'src/protocol/schema.js'), 'utf8'); } catch { return out; }
    const start = src.indexOf('export const enums');
    const end = src.indexOf('export const MoveFlag');
    if (start < 0 || end < 0) return out;
    let current = null;
    for (const line of src.slice(start, end).split('\n')) {
        const head = /^\s{4}(\w+): \{/.exec(line);
        if (head) current = head[1];
        const m = /(\w+): (\d+),?\s*\/\/\s*(.+)$/.exec(line);
        if (current && m) out[`${current}.${m[1]}`] = m[3].trim();
    }
    return out;
}

// Trailing comments of the struct fields in schema.js (documentation only; always the file on
// disk, like enumComments).
function structComments() {
    const out = {};
    let src = '';
    try { src = fs.readFileSync(path.join(ROOT, 'src/protocol/schema.js'), 'utf8'); } catch { return out; }
    const start = src.indexOf('export const structs');
    const end = src.indexOf('export const messages');
    if (start < 0 || end < 0) return out;
    let current = null;
    for (const line of src.slice(start, end).split('\n')) {
        const head = /^\s{4}(\w+): \[/.exec(line);
        if (head) current = head[1];
        const m = /\['(\w+)', '[^']+'(?:, \{[^}]*\})?\],?\s*\/\/\s*(.+)$/.exec(line);
        if (current && m) out[`${current}.${m[1]}`] = m[2].trim();
    }
    return out;
}

function fnv(s) {
    let h = 0x811c9dc5;
    for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i) & 0xff; h = Math.imul(h, 0x01000193) >>> 0; }
    return h >>> 0;
}

// ---- benchmark ---------------------------------------------------------------------------------

async function bench() {
    const gen = await import(pathToFileURL(CODEC_PATH).href);
    const ref = await import(pathToFileURL(path.join(ROOT, 'src/protocol/codec.interpreted.js')).href);
    const player = (userId, name, rating) => ({ userId, name, rating, provisional: false });
    const moves = [];
    for (let i = 0; i < 80; i++) moves.push({ move: (i * 37 + 12) & 0x7fff, spentMs: 1000 + i * 17, clockMs: 180000 - i * 1500 });
    const samples = {
        Move: { seq: 42, game: 123456789012345, ply: 17, move: 1804, posHash: 0xdeadbeef, thinkMs: 2345, drawOffer: false },
        MoveMade: { game: 123456789012345, gseq: 18, ply: 17, move: 1804, flags: 65, spentMs: 2100, whiteMs: 170000, blackMs: 165000, serverTime: 1790000000123.5, drawOffer: false, firstMoveMs: 0 },
        GameSnapshot: {
            game: 123456789012345, gseq: 80, category: '3+2', baseMs: 180000, incMs: 2000, rated: true,
            white: player(17, 'Łukasz', 1520), black: player(42, 'yuki', 1488), you: 0, moves,
            running: 0, whiteMs: 120000, blackMs: 110000, serverTime: 1790000000123.5, drawOffer: 2, status: 0, reason: 0,
            whiteConnected: true, blackConnected: true, graceMs: 18000, firstMoveMs: 0, startedAt: 1790000000000, rematch: 2,
        },
    };
    const run = (fn, ms) => {
        for (let i = 0; i < 20000; i++) fn();                    // warm-up
        let n = 0;
        const t0 = process.hrtime.bigint();
        const end = t0 + BigInt(ms) * 1000000n;
        let t = t0;
        while (t < end) { for (let i = 0; i < 1000; i++) fn(); n += 1000; t = process.hrtime.bigint(); }
        return n / (Number(t - t0) / 1e9);
    };
    const fmt = (x) => (x >= 1e6 ? `${(x / 1e6).toFixed(2)} M/s` : `${(x / 1e3).toFixed(1)} k/s`);
    console.log('| Message | interpreted (placeholder) | generated | speed-up | encode (gen) | decode (gen) |');
    console.log('|---|---:|---:|---:|---:|---:|');
    for (const [name, obj] of Object.entries(samples)) {
        const bytes = gen.encode[name](obj);
        if (!bytes.equals(ref.encode[name](obj))) throw new Error(`bench: codecs disagree on ${name}`);
        const both = (c) => () => c.decode(c.encode[name](obj));
        const r = run(both(ref), 1500);
        const g = run(both(gen), 1500);
        const ge = run(() => gen.encode[name](obj), 1000);
        const gd = run(() => gen.decode(bytes), 1000);
        console.log(`| \`${name}\` (${bytes.length} bytes) | ${fmt(r)} | ${fmt(g)} | ${(g / r).toFixed(1)}x | ${fmt(ge)} | ${fmt(gd)} |`);
    }
}

// ---- main --------------------------------------------------------------------------------------

function runChild(script, args) {
    const file = path.join(here, script);
    if (!fs.existsSync(file)) return 0;
    try {
        execFileSync(process.execPath, [file, ...args], { stdio: 'inherit', cwd: ROOT });
        return 0;
    } catch (e) {
        return e.status || 1;
    }
}

async function main(argv) {
    if (argv.includes('--bench')) { await bench(); return 0; }
    const check = argv.includes('--check');
    const outputs = [[CODEC_PATH, generateCodec()], [DOCS_PATH, generateDocs()]];
    let status = 0;
    for (const [file, text] of outputs) {
        const rel = path.relative(ROOT, file);
        const current = fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : null;
        if (check) {
            if (current !== text) { console.error(`${rel} is stale: run npm run gen:protocol`); status = 1; }
        } else if (current !== text) {
            fs.mkdirSync(path.dirname(file), { recursive: true });
            fs.writeFileSync(file, text);
            console.log(`wrote ${rel}`);
        } else console.log(`${rel} up to date`);
    }
    const childArgs = check ? ['--check'] : [];
    // Vectors are built with the codec on disk: in --check mode a stale codec already failed above.
    if (runChild('gen-protocol-vectors.js', childArgs)) status = 1;
    if (runChild('gen-protocol-cpp.js', childArgs)) status = 1;
    if (runChild('gen-cpp-test-vectors.js', childArgs)) status = 1;
    return status;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
    main(process.argv.slice(2)).then((code) => { process.exitCode = code; }, (e) => { console.error(e.message); process.exitCode = 1; });
}
