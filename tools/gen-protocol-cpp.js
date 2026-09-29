#!/usr/bin/env node
// C++17 code generator for the game client's protocol codec.
//
//   node tools/gen-protocol-cpp.js           writes ../src/net/protocol_gen.h and protocol_gen.cpp
//   node tools/gen-protocol-cpp.js --check   exits with status 1 when the committed files are stale
//
// Input: src/protocol/schema.js (the single source of truth) and computeSchemaHash() from
// src/protocol/schema-hash.js, shared with the JavaScript generator so that both sides agree on
// SCHEMA_HASH. The output implements docs/DESIGN.md section 5.1 (C++ part) with exactly the
// decoding rules of the JavaScript codec: integer ranges and opts.min / opts.max, enum
// membership, bool 0 or 1, finite f64, id53 < 2^53, str8 = valid UTF-8 without NUL within its
// length bounds, list16 count <= opts.max, exact consumption of the message.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as schema from '../src/protocol/schema.js';
import { computeSchemaHash } from '../src/protocol/schema-hash.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const outDir = path.resolve(here, '../../src/net');
const headerPath = path.join(outDir, 'protocol_gen.h');
const sourcePath = path.join(outDir, 'protocol_gen.cpp');

// ---- schema model ------------------------------------------------------------------------------

// Names used by both directions (Ping, Pong) get C_ / S_ prefixes, as in the JS codec.
const counts = new Map();
for (const m of schema.messages) counts.set(m.name, (counts.get(m.name) || 0) + 1);
const cppName = (m) => (counts.get(m.name) > 1 ? (m.dir === 'c2s' ? 'C_' : 'S_') + m.name : m.name);

function parseType(t) {
    if (t.startsWith('list16:')) return { kind: 'list', item: parseType(t.slice(7)) };
    if (t.startsWith('struct:')) {
        const name = t.slice(7);
        if (!schema.structs[name]) throw new Error(`unknown struct ${name}`);
        return { kind: 'struct', name };
    }
    if (t.startsWith('enum:')) {
        const name = t.slice(5);
        if (!schema.enums[name]) throw new Error(`unknown enum ${name}`);
        return { kind: 'enum', name };
    }
    if (!['u8', 'u16', 'u32', 'i32', 'f64', 'id53', 'bool', 'str8'].includes(t)) throw new Error(`unknown type ${t}`);
    return { kind: t };
}
const fieldsOf = (list) => list.map(([name, type, opts = {}]) => ({ name, t: parseType(type), opts }));

const INT_RANGE = { u8: [0, 0xff], u16: [0, 0xffff], u32: [0, 0xffffffff], i32: [-0x80000000, 0x7fffffff] };
const CPP_SCALAR = { u8: 'uint8_t', u16: 'uint16_t', u32: 'uint32_t', i32: 'int32_t', f64: 'double', id53: 'uint64_t', bool: 'bool', str8: 'std::string' };

function cppType(t) {
    if (t.kind === 'list') return `std::vector<${cppType(t.item)}>`;
    if (t.kind === 'struct' || t.kind === 'enum') return t.name;
    return CPP_SCALAR[t.kind];
}

function cppDefault(t) {
    switch (t.kind) {
        case 'u8': case 'u16': case 'u32': case 'i32': case 'id53': return ' = 0';
        case 'f64': return ' = 0.0';
        case 'bool': return ' = false';
        case 'enum': return ` = ${t.name}::${Object.keys(schema.enums[t.name])[0]}`;
        default: return '';
    }
}

// Enums travel as u8. A schema enum with a value above 255 cannot be sent; its C++ type is made
// wide enough to compile (encode writes value & 0xFF like the JS codec, decode refuses it).
function wideValues(name) { return Object.entries(schema.enums[name]).filter(([, v]) => v > 255).map(([k, v]) => `${k}=${v}`); }
function enumUnderlying(name) { return wideValues(name).length ? 'uint16_t' : 'uint8_t'; }
for (const name of Object.keys(schema.enums)) {
    if (wideValues(name).length) console.warn(`warning: enum ${name} has values above 255 (${wideValues(name).join(', ')}): not encodable as u8`);
}

function intLit(v, kind) {
    if (kind === 'i32') return v === -0x80000000 ? '-2147483647 - 1' : String(v);
    return v > 0xffff ? `0x${v.toString(16)}u` : `${v}u`;
}

// Integer bounds of a field: the type's range narrowed by opts.
function bounds(f) {
    const [lo, hi] = INT_RANGE[f.t.kind];
    return [f.opts.min ?? lo, f.opts.max ?? hi];
}

function strMax(f) { return Math.min(f.opts.max ?? 255, 255); }
function strMin(f) { return f.opts.min ?? 0; }
function listMax(f) { return Math.min(f.opts.max ?? 0xffff, 0xffff); }

// ---- code emitters -----------------------------------------------------------------------------

// Writer statement for value expression `v`.
function emitPut(t, v, f, ind) {
    switch (t.kind) {
        case 'u8': return `${ind}w.u8(${v});`;
        case 'bool': return `${ind}w.u8(${v} ? 1 : 0);`;
        case 'enum': return `${ind}w.u8(uint8_t(${v}));`;
        case 'u16': return `${ind}w.u16(${v});`;
        case 'u32': return `${ind}w.u32(${v});`;
        case 'i32': return `${ind}w.u32(uint32_t(${v}));`;
        case 'f64': return `${ind}w.f64(${v});`;
        case 'id53': return `${ind}w.u64(${v});`;
        case 'str8': return `${ind}w.str8(${v}, ${strMax(f)});`;
        case 'struct': return `${ind}put(w, ${v});`;
        case 'list': {
            const max = listMax(f);
            return [
                `${ind}{`,
                `${ind}    const size_t count = ${v}.size() < ${max} ? ${v}.size() : ${max};`,
                `${ind}    w.u16(uint16_t(count));`,
                `${ind}    for (size_t i = 0; i < count; ++i) {`,
                emitPut(t.item, `${v}[i]`, { opts: {} }, ind + '        '),
                `${ind}    }`,
                `${ind}}`,
            ].join('\n');
        }
    }
    throw new Error(`put ${t.kind}`);
}

// Reader expression (bool) that reads into lvalue `v`.
function emitGet(t, v, f) {
    switch (t.kind) {
        case 'u8': case 'u16': case 'u32': case 'i32': {
            const [lo, hi] = bounds(f);
            return `r.${t.kind}(${v}, ${intLit(lo, t.kind)}, ${intLit(hi, t.kind)})`;
        }
        case 'bool': return `r.boolean(${v})`;
        case 'enum': return `r.enumeration(${v})`;
        case 'f64': return `r.f64(${v})`;
        case 'id53': return `r.id53(${v})`;
        case 'str8': return `r.str8(${v}, ${strMin(f)}, ${strMax(f)})`;
        case 'struct': return `get(r, ${v})`;
        case 'list': {
            const item = cppType(t.item);
            return `r.list(${v}, ${listMax(f)}, [](Reader& r, ${item}& x) { return ${emitGet(t.item, 'x', { opts: {} })}; })`;
        }
    }
    throw new Error(`get ${t.kind}`);
}

// Validity expression for value `v` (null when the C++ type cannot hold an invalid value).
function emitCheck(t, v, f) {
    switch (t.kind) {
        case 'u8': case 'u16': case 'u32': case 'i32': {
            const parts = [];
            if (f.opts.min !== undefined) parts.push(`${v} >= ${intLit(f.opts.min, t.kind)}`);
            if (f.opts.max !== undefined) parts.push(`${v} <= ${intLit(f.opts.max, t.kind)}`);
            return parts.length ? parts.join(' && ') : null;
        }
        case 'bool': return null;
        case 'enum': return `isValid(${v})`;
        case 'f64': return `std::isfinite(${v})`;
        case 'id53': return `${v} < kId53Limit`;
        case 'str8': return `validStr(${v}, ${strMin(f)}, ${strMax(f)})`;
        case 'struct': return `valid(${v})`;
        case 'list': {
            const item = emitCheck(t.item, 'x', { opts: {} });
            const size = `${v}.size() <= ${listMax(f)}`;
            if (!item) return size;
            return `${size} && allOf(${v}, [](const ${cppType(t.item)}& x) { return ${item}; })`;
        }
    }
    throw new Error(`check ${t.kind}`);
}

function docComment(text, ind) {
    if (!text) return [];
    const words = text.split(/\s+/);
    const lines = [];
    let cur = '';
    for (const w of words) {
        if ((cur + ' ' + w).length > 96 - ind.length) { lines.push(cur); cur = w; } else cur = cur ? cur + ' ' + w : w;
    }
    if (cur) lines.push(cur);
    return lines.map((l) => `${ind}// ${l}`);
}

function trailingComment(opts) {
    const parts = [];
    if (opts.min !== undefined) parts.push(`min ${opts.min}`);
    if (opts.max !== undefined) parts.push(`max ${opts.max}`);
    return parts.length ? `  // ${parts.join(', ')}` : '';
}

// ---- header ------------------------------------------------------------------------------------

function genHeader(hash) {
    const L = [];
    L.push('// Scacelith realtime protocol codec (C++17): generated from dedicated-server/src/protocol/schema.js');
    L.push('// by dedicated-server/tools/gen-protocol-cpp.js, do not edit. Regenerate with');
    L.push('// `node dedicated-server/tools/gen-protocol-cpp.js` after changing the schema.');
    L.push('//');
    L.push('// Wire format and field meanings: schema.js; API: dedicated-server/docs/DESIGN.md section 5.1.');
    L.push('//   encode(m, out)       appends the message (type byte + fields, little-endian) to out. Strings');
    L.push('//                        longer than their bound are cut and lists longer than theirs are');
    L.push('//                        shortened so the frame stays well formed; check valid(m) first when');
    L.push('//                        the values come from user input.');
    L.push('//   decode(p, n, out)    false on any malformed input (wrong type byte, truncation, trailing');
    L.push('//                        bytes, value out of range or opts bounds, unknown enum value, bool not');
    L.push('//                        0/1, non-finite f64, id53 >= 2^53, string not UTF-8 or with NUL or');
    L.push('//                        outside its length bounds, list too long). out may be partly written.');
    L.push('//   valid(m)             true when decode(encode(m)) would succeed and give m back.');
    L.push('//   peekType(p, n, t)    type of a message (false when empty or unknown).');
    L.push('#pragma once');
    L.push('#include <cstddef>');
    L.push('#include <cstdint>');
    L.push('#include <string>');
    L.push('#include <vector>');
    L.push('');
    L.push('// X11 defines None as a macro; enum values below are called None.');
    L.push('#pragma push_macro("None")');
    L.push('#undef None');
    L.push('');
    L.push('namespace net {');
    L.push('namespace proto {');
    L.push('');
    L.push(`constexpr uint16_t kProtocolVersion = ${schema.PROTOCOL_VERSION};`);
    L.push(`constexpr uint16_t kProtocolMin = ${schema.PROTOCOL_MIN};`);
    L.push(`constexpr uint32_t kSchemaHash = 0x${hash.toString(16).padStart(8, '0')}u;`);
    L.push(`constexpr const char* kWsSubprotocol = "${schema.WS_SUBPROTOCOL}";`);
    L.push('constexpr uint64_t kId53Limit = 1ull << 53;   // id53 values are below 2^53');
    L.push('');
    L.push('// ---- enums (u8 on the wire) ----');
    for (const [name, values] of Object.entries(schema.enums)) {
        const items = Object.entries(values).map(([k, v]) => `${k} = ${v}`);
        const under = enumUnderlying(name);
        if (under !== 'uint8_t') {
            L.push(`// SCHEMA BUG: ${name} has values above 255 (${wideValues(name).join(', ')})`);
            L.push('// but enums are u8 on the wire: those values cannot be sent (the frame carries value & 0xFF,');
            L.push('// which decode refuses). The C++ type is wider only so that this header compiles.');
        }
        const one = `enum class ${name} : ${under} { ${items.join(', ')} };`;
        if (one.length <= 110) L.push(one);
        else {
            L.push(`enum class ${name} : ${under} {`);
            let line = '   ';
            for (const it of items) {
                if ((line + ' ' + it + ',').length > 104) { L.push(line); line = '   '; }
                line += ' ' + it + ',';
            }
            L.push(line);
            L.push('};');
        }
    }
    L.push('');
    L.push('// Membership of the schema enums, and their value names ("?" when not a member).');
    for (const name of Object.keys(schema.enums)) L.push(`bool isValid(${name} v);`);
    for (const name of Object.keys(schema.enums)) L.push(`const char* enumName(${name} v);`);
    L.push('');
    L.push('// Move flags (MoveMade.flags, bit set).');
    L.push('namespace MoveFlag {');
    for (const [k, v] of Object.entries(schema.MoveFlag)) L.push(`constexpr uint8_t ${k} = ${v};`);
    L.push('}  // namespace MoveFlag');
    L.push('');
    L.push('// Gesture flags (Gesture.flags, bit set).');
    L.push('namespace GestureFlag {');
    for (const [k, v] of Object.entries(schema.GestureFlag)) L.push(`constexpr uint8_t ${k} = ${v};`);
    L.push('}  // namespace GestureFlag');
    L.push('');
    L.push('// WebSocket close codes used by the server (4000 + ErrorCode where one applies).');
    L.push('namespace CloseCode {');
    for (const [k, v] of Object.entries(schema.CloseCode)) L.push(`constexpr uint16_t ${k} = ${v};`);
    L.push('}  // namespace CloseCode');
    L.push('');
    L.push('// ---- message types (0x01-0x7F client -> server, 0x80-0xFF server -> client) ----');
    L.push('enum class MsgType : uint8_t {');
    for (const m of schema.messages) L.push(`    ${cppName(m)} = 0x${m.id.toString(16).toUpperCase().padStart(2, '0')},`);
    L.push('};');
    L.push('const char* messageName(MsgType t);     // "Move", "S_Ping"...; nullptr when unknown');
    L.push('inline bool isClientType(uint8_t t) { return t >= 0x01 && t <= 0x7F; }');
    L.push('bool peekType(const uint8_t* p, size_t n, MsgType& t);');
    L.push('');
    L.push('// ---- structs ----');
    for (const [name, list] of Object.entries(schema.structs)) {
        L.push(`struct ${name} {`);
        for (const f of fieldsOf(list)) L.push(`    ${cppType(f.t)} ${f.name}${cppDefault(f.t)};${trailingComment(f.opts)}`);
        L.push('};');
    }
    L.push('');
    L.push('// ---- messages ----');
    for (const m of schema.messages) {
        const n = cppName(m);
        L.push(...docComment(m.doc, ''));
        L.push(`struct ${n} {`);
        L.push(`    static constexpr MsgType kType = MsgType::${n};`);
        L.push(`    static constexpr bool kClientToServer = ${m.dir === 'c2s'};`);
        for (const f of fieldsOf(m.fields)) L.push(`    ${cppType(f.t)} ${f.name}${cppDefault(f.t)};${trailingComment(f.opts)}`);
        L.push('};');
    }
    L.push('');
    L.push('// ---- codec ----');
    for (const name of Object.keys(schema.structs)) L.push(`bool valid(const ${name}& s);`);
    for (const m of schema.messages) {
        const n = cppName(m);
        L.push(`void encode(const ${n}& m, std::vector<uint8_t>& out);`);
        L.push(`bool decode(const uint8_t* p, size_t n, ${n}& out);`);
        L.push(`bool valid(const ${n}& m);`);
    }
    L.push('');
    L.push('// ---- reflection (tests, logs): v(name, field) for every field, in wire order ----');
    const visit = (n, list) => {
        for (const cq of ['', 'const ']) {
            L.push(`template <class V> void visitFields(${cq}${n}& m, V&& v) {`);
            for (const f of fieldsOf(list)) L.push(`    v("${f.name}", m.${f.name});`);
            L.push('}');
        }
    };
    for (const [name, list] of Object.entries(schema.structs)) visit(name, list);
    for (const m of schema.messages) visit(cppName(m), m.fields);
    L.push('');
    L.push('// Calls f(msg) with a default-constructed message of type t; false when t is unknown.');
    L.push('template <class F> bool withMessage(MsgType t, F&& f) {');
    L.push('    switch (t) {');
    for (const m of schema.messages) L.push(`    case MsgType::${cppName(m)}: { ${cppName(m)} m; f(m); return true; }`);
    L.push('    }');
    L.push('    return false;');
    L.push('}');
    L.push('');
    L.push('}  // namespace proto');
    L.push('}  // namespace net');
    L.push('');
    L.push('#pragma pop_macro("None")');
    L.push('');
    return L.join('\n');
}

// ---- source ------------------------------------------------------------------------------------

const RUNTIME = String.raw`namespace {

// Appends little-endian values.
class Writer {
public:
    explicit Writer(std::vector<uint8_t>& out) : o_(out) {}
    void u8(uint8_t v) { o_.push_back(v); }
    void u16(uint16_t v) { o_.push_back(uint8_t(v)); o_.push_back(uint8_t(v >> 8)); }
    void u32(uint32_t v) { for (int i = 0; i < 4; ++i) o_.push_back(uint8_t(v >> (8 * i))); }
    void u64(uint64_t v) { for (int i = 0; i < 8; ++i) o_.push_back(uint8_t(v >> (8 * i))); }
    void f64(double v) {
        uint64_t bits;
        std::memcpy(&bits, &v, 8);
        u64(bits);
    }
    // Cut to 'max' bytes without splitting a UTF-8 sequence (only reached with invalid input).
    void str8(const std::string& s, size_t max) {
        size_t n = s.size();
        if (n > max) {
            n = max;
            while (n > 0 && (uint8_t(s[n]) & 0xC0) == 0x80) --n;
        }
        u8(uint8_t(n));
        o_.insert(o_.end(), s.begin(), s.begin() + std::ptrdiff_t(n));
    }

private:
    std::vector<uint8_t>& o_;
};

// Strict UTF-8 (no overlong forms, no surrogates, <= U+10FFFF), and no NUL byte.
bool utf8NoNul(const uint8_t* s, size_t n) {
    size_t i = 0;
    while (i < n) {
        uint8_t c = s[i];
        if (c == 0) return false;
        if (c < 0x80) { ++i; continue; }
        size_t len;
        uint32_t cp, min;
        if ((c & 0xE0) == 0xC0) { len = 2; cp = c & 0x1F; min = 0x80; }
        else if ((c & 0xF0) == 0xE0) { len = 3; cp = c & 0x0F; min = 0x800; }
        else if ((c & 0xF8) == 0xF0) { len = 4; cp = c & 0x07; min = 0x10000; }
        else return false;
        if (i + len > n) return false;
        for (size_t k = 1; k < len; ++k) {
            uint8_t cc = s[i + k];
            if ((cc & 0xC0) != 0x80) return false;
            cp = (cp << 6) | (cc & 0x3F);
        }
        if (cp < min || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) return false;
        i += len;
    }
    return true;
}

bool validStr(const std::string& s, size_t min, size_t max) {
    return s.size() >= min && s.size() <= max && utf8NoNul(reinterpret_cast<const uint8_t*>(s.data()), s.size());
}

template <class T, class P> bool allOf(const std::vector<T>& v, P pred) {
    for (const T& x : v)
        if (!pred(x)) return false;
    return true;
}

// Bounds-checked little-endian reader; every accessor fails on truncation or a bad value.
class Reader {
public:
    Reader(const uint8_t* p, size_t n) : p_(p), n_(n) {}
    bool type(MsgType t) {
        if (n_ < 1 || p_[0] != uint8_t(t)) return false;
        o_ = 1;
        return true;
    }
    bool end() const { return o_ == n_; }
    bool u8(uint8_t& v, uint32_t lo, uint32_t hi) {
        if (n_ - o_ < 1) return false;
        v = p_[o_++];
        return v >= lo && v <= hi;
    }
    bool u16(uint16_t& v, uint32_t lo, uint32_t hi) {
        if (n_ - o_ < 2) return false;
        v = uint16_t(p_[o_] | (p_[o_ + 1] << 8));
        o_ += 2;
        return v >= lo && v <= hi;
    }
    bool u32(uint32_t& v, uint32_t lo, uint32_t hi) {
        if (!raw32(v)) return false;
        return v >= lo && v <= hi;
    }
    bool i32(int32_t& v, int32_t lo, int32_t hi) {
        uint32_t u;
        if (!raw32(u)) return false;
        v = int32_t(u);
        return v >= lo && v <= hi;
    }
    bool id53(uint64_t& v) {
        if (!raw64(v)) return false;
        return v < kId53Limit;
    }
    bool f64(double& v) {
        uint64_t bits;
        if (!raw64(bits)) return false;
        std::memcpy(&v, &bits, 8);
        return std::isfinite(v);
    }
    bool boolean(bool& v) {
        if (n_ - o_ < 1) return false;
        uint8_t b = p_[o_++];
        v = b == 1;
        return b <= 1;
    }
    template <class E> bool enumeration(E& v) {
        if (n_ - o_ < 1) return false;
        v = E(p_[o_++]);
        return isValid(v);
    }
    bool str8(std::string& v, size_t min, size_t max) {
        if (n_ - o_ < 1) return false;
        size_t len = p_[o_++];
        if (len < min || len > max || n_ - o_ < len) return false;
        if (!utf8NoNul(p_ + o_, len)) return false;
        v.assign(reinterpret_cast<const char*>(p_ + o_), len);
        o_ += len;
        return true;
    }
    template <class T, class F> bool list(std::vector<T>& v, size_t max, F item) {
        uint16_t count;
        if (!u16(count, 0, 0xFFFF) || count > max) return false;
        v.clear();
        v.resize(count);
        for (T& x : v)
            if (!item(*this, x)) return false;
        return true;
    }

private:
    bool raw32(uint32_t& v) {
        if (n_ - o_ < 4) return false;
        v = uint32_t(p_[o_]) | uint32_t(p_[o_ + 1]) << 8 | uint32_t(p_[o_ + 2]) << 16 | uint32_t(p_[o_ + 3]) << 24;
        o_ += 4;
        return true;
    }
    bool raw64(uint64_t& v) {
        uint32_t lo, hi;
        if (!raw32(lo) || !raw32(hi)) return false;
        v = uint64_t(hi) << 32 | lo;
        return true;
    }
    const uint8_t* p_;
    size_t n_, o_ = 0;
};

}  // namespace`;

function genSource(hash) {
    const L = [];
    L.push('// Scacelith realtime protocol codec (C++17): generated from dedicated-server/src/protocol/schema.js');
    L.push('// by dedicated-server/tools/gen-protocol-cpp.js, do not edit.');
    L.push('#include "protocol_gen.h"');
    L.push('#include <cmath>');
    L.push('#include <cstring>');
    L.push('');
    L.push('#pragma push_macro("None")');
    L.push('#undef None');
    L.push('');
    L.push('namespace net {');
    L.push('namespace proto {');
    L.push('');
    L.push('// ---- enums ----');
    for (const [name, values] of Object.entries(schema.enums)) {
        L.push(`bool isValid(${name} v) {`);
        L.push('    switch (v) {');
        for (const k of Object.keys(values)) L.push(`    case ${name}::${k}:`);
        L.push('        return true;');
        L.push('    }');
        L.push('    return false;');
        L.push('}');
        L.push(`const char* enumName(${name} v) {`);
        L.push('    switch (v) {');
        for (const k of Object.keys(values)) L.push(`    case ${name}::${k}: return "${k}";`);
        L.push('    }');
        L.push('    return "?";');
        L.push('}');
    }
    L.push('');
    L.push('const char* messageName(MsgType t) {');
    L.push('    switch (t) {');
    for (const m of schema.messages) L.push(`    case MsgType::${cppName(m)}: return "${cppName(m)}";`);
    L.push('    }');
    L.push('    return nullptr;');
    L.push('}');
    L.push('');
    L.push('bool peekType(const uint8_t* p, size_t n, MsgType& t) {');
    L.push('    if (!p || n < 1 || !messageName(MsgType(p[0]))) return false;');
    L.push('    t = MsgType(p[0]);');
    L.push('    return true;');
    L.push('}');
    L.push('');
    L.push(RUNTIME);
    L.push('');
    L.push('// ---- structs ----');
    L.push('namespace {');
    for (const name of Object.keys(schema.structs)) {
        L.push(`void put(Writer& w, const ${name}& s);`);
        L.push(`bool get(Reader& r, ${name}& s);`);
    }
    L.push('');
    for (const [name, list] of Object.entries(schema.structs)) {
        const fields = fieldsOf(list);
        L.push(`void put(Writer& w, const ${name}& s) {`);
        for (const f of fields) L.push(emitPut(f.t, `s.${f.name}`, f, '    '));
        L.push('}');
        L.push(`bool get(Reader& r, ${name}& s) {`);
        L.push(`    return ${fields.map((f) => emitGet(f.t, `s.${f.name}`, f)).join(' &&\n           ')};`);
        L.push('}');
    }
    L.push('}  // namespace');
    L.push('');
    for (const [name, list] of Object.entries(schema.structs)) {
        const checks = fieldsOf(list).map((f) => emitCheck(f.t, `s.${f.name}`, f)).filter(Boolean);
        L.push(`bool valid(const ${name}& s) {`);
        L.push(checks.length ? `    return ${checks.join(' &&\n           ')};` : '    (void)s;\n    return true;');
        L.push('}');
    }
    L.push('');
    L.push('// ---- messages ----');
    for (const m of schema.messages) {
        const n = cppName(m);
        const fields = fieldsOf(m.fields);
        L.push(`void encode(const ${n}& m, std::vector<uint8_t>& out) {`);
        L.push('    Writer w(out);');
        L.push(`    w.u8(uint8_t(MsgType::${n}));`);
        for (const f of fields) L.push(emitPut(f.t, `m.${f.name}`, f, '    '));
        L.push('}');
        L.push(`bool decode(const uint8_t* p, size_t n, ${n}& out) {`);
        L.push('    Reader r(p, n);');
        const gets = [`r.type(MsgType::${n})`, ...fields.map((f) => emitGet(f.t, `out.${f.name}`, f)), 'r.end()'];
        L.push(`    return ${gets.join(' &&\n           ')};`);
        L.push('}');
        const checks = fields.map((f) => emitCheck(f.t, `m.${f.name}`, f)).filter(Boolean);
        L.push(`bool valid(const ${n}& m) {`);
        L.push(checks.length ? `    return ${checks.join(' &&\n           ')};` : '    (void)m;\n    return true;');
        L.push('}');
    }
    L.push('');
    L.push('}  // namespace proto');
    L.push('}  // namespace net');
    L.push('');
    L.push('#pragma pop_macro("None")');
    L.push('');
    return L.join('\n');
}

// ---- main --------------------------------------------------------------------------------------

const hash = computeSchemaHash();
const header = genHeader(hash);
const source = genSource(hash);

if (process.argv.includes('--check')) {
    const read = (p) => (fs.existsSync(p) ? fs.readFileSync(p, 'utf8') : '');
    const stale = [[headerPath, header], [sourcePath, source]].filter(([p, s]) => read(p) !== s).map(([p]) => p);
    if (stale.length) {
        console.error(`stale generated files (run node tools/gen-protocol-cpp.js): ${stale.join(', ')}`);
        process.exit(1);
    }
    console.log(`protocol_gen.* up to date (schema hash 0x${hash.toString(16).padStart(8, '0')})`);
} else {
    fs.mkdirSync(outDir, { recursive: true });
    fs.writeFileSync(headerPath, header);
    fs.writeFileSync(sourcePath, source);
    console.log(`wrote ${path.relative(process.cwd(), headerPath)} and ${path.relative(process.cwd(), sourcePath)} (schema hash 0x${hash.toString(16).padStart(8, '0')})`);
}
