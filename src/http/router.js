// HTTP router of the API (docs/DESIGN.md section 5.9) and its declarative body validator.
//
//   router.get(path, handler, { auth: 'none'|'optional'|'required', rate, query })
//   router.post(path, handler, { auth, body: schema, rate })      // also put / patch / delete
//   router.page(method, path, handler, opts)                      // HTML page outside /api
//   handler(ctx) -> { status, body, headers } | { status, html, headers }
//
// Paths: a path that starts with "/api/" is taken as it is; any other path is relative to the
// API prefix "/api/v1" (so route modules may write either '/players/:username' or
// '/api/v1/players/:username'), except page routes ({ page: true } or router.page()), which are
// absolute paths outside /api (e.g. '/verify-email'). Parameters are ":name" segments; a literal
// segment wins over a parameter when both match.
//
// rate: { key, limit, windowMs, shared?: bool, by?: 'ip'|'user' } or an array of them. `shared`
//   limits also go through the primary (`ratelimit.take`) after the local token bucket, so they
//   hold across every shard (used for the auth-sensitive endpoints).
//
// Body schemas (strict: unknown fields are refused, fields are required unless `optional`):
//   { field: { type: 'string', min, max, maxBytes, pattern, multiline, optional, nullable },
//     n: { type: 'integer'|'number', min, max }, b: { type: 'boolean' },
//     e: { type: 'enum', values: [...] }, o: { type: 'object', fields: {...} },
//     a: { type: 'array', items: spec, min, max } }
// Strings refuse control characters (except \t \r \n with `multiline: true`).

export const API_PREFIX = '/api/v1';

/** An error answered as JSON `{ error: code, message, ...extra }` with HTTP `status`. */
export class HttpError extends Error {
    /**
     * @param {number} status
     * @param {string} code snake_case error code
     * @param {string} [message]
     * @param {object} [extra] fields merged into the JSON answer (retryAfter, pow...)
     * @param {object} [headers]
     */
    constructor(status, code, message, extra, headers) {
        super(message || code);
        this.status = status;
        this.code = code;
        this.extra = extra || null;
        this.headers = headers || null;
        this.expose = true;
    }
}

const CONTROL = /[\u0000-\u001f\u007f]/;
const CONTROL_MULTILINE = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/;

function checkValue(spec, v, name) {
    if (v === null) return spec.nullable ? { ok: true, value: null } : { ok: false, field: name, message: `"${name}" must not be null` };
    switch (spec.type) {
        case 'string': {
            if (typeof v !== 'string') return { ok: false, field: name, message: `"${name}" must be a string` };
            if (spec.min !== undefined && v.length < spec.min) return { ok: false, field: name, message: `"${name}" is too short (at least ${spec.min} characters)` };
            if (spec.max !== undefined && v.length > spec.max) return { ok: false, field: name, message: `"${name}" is too long (at most ${spec.max} characters)` };
            if (spec.maxBytes !== undefined && Buffer.byteLength(v, 'utf8') > spec.maxBytes) return { ok: false, field: name, message: `"${name}" is too long` };
            if ((spec.multiline ? CONTROL_MULTILINE : CONTROL).test(v)) return { ok: false, field: name, message: `"${name}" contains control characters` };
            if (spec.pattern && !spec.pattern.test(v)) return { ok: false, field: name, message: `"${name}" has an invalid format` };
            return { ok: true, value: v };
        }
        case 'integer':
        case 'number': {
            if (typeof v !== 'number' || !Number.isFinite(v) || (spec.type === 'integer' && !Number.isSafeInteger(v))) {
                return { ok: false, field: name, message: `"${name}" must be ${spec.type === 'integer' ? 'an integer' : 'a number'}` };
            }
            if (spec.min !== undefined && v < spec.min) return { ok: false, field: name, message: `"${name}" must be at least ${spec.min}` };
            if (spec.max !== undefined && v > spec.max) return { ok: false, field: name, message: `"${name}" must be at most ${spec.max}` };
            return { ok: true, value: v };
        }
        case 'boolean':
            return typeof v === 'boolean' ? { ok: true, value: v } : { ok: false, field: name, message: `"${name}" must be true or false` };
        case 'enum':
            return spec.values.includes(v) ? { ok: true, value: v } : { ok: false, field: name, message: `"${name}" must be one of ${spec.values.join(', ')}` };
        case 'object':
            return validateObject(spec.fields || {}, v, name + '.');
        case 'array': {
            if (!Array.isArray(v)) return { ok: false, field: name, message: `"${name}" must be an array` };
            if (spec.min !== undefined && v.length < spec.min) return { ok: false, field: name, message: `"${name}" needs at least ${spec.min} items` };
            if (spec.max !== undefined && v.length > spec.max) return { ok: false, field: name, message: `"${name}" has too many items` };
            const out = [];
            for (let i = 0; i < v.length; i++) {
                const r = checkValue(spec.items, v[i], `${name}[${i}]`);
                if (!r.ok) return r;
                out.push(r.value);
            }
            return { ok: true, value: out };
        }
        default:
            throw new Error(`schema: unknown type ${spec.type} for ${name}`);
    }
}

function validateObject(fields, value, prefix = '') {
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
        return { ok: false, field: prefix.replace(/\.$/, '') || null, message: prefix ? `"${prefix.slice(0, -1)}" must be an object` : 'the body must be a JSON object' };
    }
    for (const k of Object.keys(value)) {
        if (!Object.prototype.hasOwnProperty.call(fields, k)) return { ok: false, field: prefix + k, message: `unknown field "${prefix + k}"` };
    }
    const out = {};
    for (const [k, spec] of Object.entries(fields)) {
        const v = value[k];
        if (v === undefined) {
            if (spec.optional) continue;
            return { ok: false, field: prefix + k, message: `"${prefix + k}" is required` };
        }
        const r = checkValue(spec, v, prefix + k);
        if (!r.ok) return r;
        out[k] = r.value;
    }
    return { ok: true, value: out };
}

/**
 * Validates a parsed JSON body (or query object) against a schema.
 * @param {object} schema field name -> spec
 * @param {unknown} value
 * @returns {{ ok: true, value: object } | { ok: false, field: string|null, message: string }}
 */
export function validate(schema, value) {
    return validateObject(schema || {}, value);
}

function compile(path) {
    const segs = path.split('/').slice(1).map((s) => (s.startsWith(':') ? { param: s.slice(1) } : { lit: s }));
    return segs;
}

/** Method + path router. */
export class Router {
    constructor({ prefix = API_PREFIX } = {}) {
        this.prefix = prefix;
        this.routes = [];
    }

    /**
     * Registers a route.
     * @param {string} method GET, POST, PUT, PATCH or DELETE
     * @param {string} path
     * @param {(ctx: object) => Promise<object>|object} handler
     * @param {object} [opts] { auth, body, query, rate, page, timeoutMs, form }
     */
    add(method, path, handler, opts = {}) {
        if (typeof handler !== 'function') throw new TypeError(`route ${method} ${path}: handler must be a function`);
        if (!path.startsWith('/')) throw new Error(`route ${path}: path must start with /`);
        const page = !!opts.page;
        const full = page || path.startsWith('/api/') || path === '/api' ? path : this.prefix + (path === '/' ? '' : path);
        const m = method.toUpperCase();
        if (this.routes.some((r) => r.method === m && r.path === full)) throw new Error(`route ${m} ${full} registered twice`);
        this.routes.push({ method: m, path: full, segs: compile(full), handler, opts: { auth: 'none', ...opts, page } });
        return this;
    }
    get(path, handler, opts) { return this.add('GET', path, handler, opts); }
    post(path, handler, opts) { return this.add('POST', path, handler, opts); }
    put(path, handler, opts) { return this.add('PUT', path, handler, opts); }
    patch(path, handler, opts) { return this.add('PATCH', path, handler, opts); }
    delete(path, handler, opts) { return this.add('DELETE', path, handler, opts); }
    /** An HTML page outside /api (form bodies, HTML errors). */
    page(method, path, handler, opts = {}) { return this.add(method, path, handler, { ...opts, page: true }); }

    /**
     * Finds the route of a request.
     * @param {string} method
     * @param {string} pathname decoded? no: the raw pathname of the URL
     * @returns {null | { route: object, params: object } | { methods: string[] }}
     *   null: no such path; { methods }: the path exists for other methods only.
     */
    match(method, pathname) {
        let p = pathname;
        if (p.length > 1 && p.endsWith('/')) p = p.slice(0, -1);
        const parts = p.split('/').slice(1);
        const m = method === 'HEAD' ? 'GET' : method;
        let best = null;
        const methods = new Set();
        for (const r of this.routes) {
            if (r.segs.length !== parts.length) continue;
            let ok = true;
            for (let i = 0; i < parts.length; i++) {
                const s = r.segs[i];
                if (s.lit !== undefined ? s.lit !== parts[i] : parts[i] === '') { ok = false; break; }
            }
            if (!ok) continue;
            methods.add(r.method);
            if (r.method !== m) continue;
            if (!best || moreSpecific(r, best)) best = r;
        }
        if (!best) return methods.size ? { methods: [...methods] } : null;
        const params = {};
        for (let i = 0; i < parts.length; i++) {
            const s = best.segs[i];
            if (s.param !== undefined) {
                try { params[s.param] = decodeURIComponent(parts[i]); } catch {
                    throw new HttpError(400, 'invalid_request', 'malformed path');
                }
            }
        }
        return { route: best, params };
    }
}

function moreSpecific(a, b) {
    for (let i = 0; i < a.segs.length; i++) {
        const la = a.segs[i].lit !== undefined, lb = b.segs[i].lit !== undefined;
        if (la !== lb) return la;
    }
    return false;
}
