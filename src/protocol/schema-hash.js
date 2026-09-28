// SCHEMA_HASH: first 4 bytes (big-endian u32) of SHA-256 over the canonical JSON of the schema
// ({version, enums, structs, messages}, object keys sorted recursively, `doc` fields removed).
// Both code generators (JS and C++) and the runtime use this function, so a client and a server
// built from different schema files detect it at Hello even when PROTOCOL_VERSION is equal.

import crypto from 'node:crypto';
import * as schema from './schema.js';

function canonical(v) {
    if (Array.isArray(v)) return v.map(canonical);
    if (v && typeof v === 'object') {
        const o = {};
        for (const k of Object.keys(v).sort()) if (k !== 'doc') o[k] = canonical(v[k]);
        return o;
    }
    return v;
}

export function schemaCanonicalJson(s = schema) {
    return JSON.stringify(canonical({ version: s.PROTOCOL_VERSION, enums: s.enums, structs: s.structs, messages: s.messages }));
}

export function computeSchemaHash(s = schema) {
    return crypto.createHash('sha256').update(schemaCanonicalJson(s)).digest().readUInt32BE(0);
}
