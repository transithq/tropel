// ══════════════════════════════════════════════════════════════════
// k6 Shim — provides the standard k6 JavaScript API for Tropel
//
// This shim defines the global symbols that k6 scripts expect:
//   http, check, group, sleep, fail, __VU, __ITER
//   Counter, Gauge, Rate, Trend (k6/metrics)
//
// Under the hood it delegates HTTP calls to __tropel_k6_http_request
// (registered lazily by the K6DriverInstance on first iteration) or
// to __tropel_pm_send_request (if the PM bridge is available).
//
// The preprocessor strips `import ... from "k6/..."` lines, so
// k6 scripts that use module syntax resolve to these globals.
// ══════════════════════════════════════════════════════════════════

// ── Globals set by native host (K6DriverInstance) ──
// __tropel_k6_http_request(method, url, headers_json, body, timeout_ms, response_type) -> native JS object (the PM fallback bridge returns a JSON string)
// __tropel_pm_send_request(method, url, headers_json, body, timeout_ms, response_type) -> JSON string
// __tropel_vu_id, __tropel_iteration_num, __tropel_scenario

// ══════════════════════════════════════════════════════════════════
// HTTP helper — tries native k6 bridge, falls back to PM bridge
// ══════════════════════════════════════════════════════════════════

function k6HTTPRequest(method, url, body, params) {
    var canonical = normalizeK6Request(method, url, body, params);
    var headersJson = JSON.stringify(canonical.headers);
    var resultJson = null;

    // Try the k6-native HTTP bridge first (lazy-registered by K6DriverInstance).
    // Both bridges accept the k6 responseType ("text"/"binary"/"none") as
    // their 6th argument. NOTE: previously this function had a duplicated
    // leftover block calling with undefined `bodyStr`/`timeoutMs` that threw
    // ReferenceError on EVERY request — removed.
    if (typeof __tropel_k6_http_request === 'function') {
        resultJson = __tropel_k6_http_request(
            canonical.method,
            canonical.url,
            headersJson,
            canonical.body,
            canonical.responseType,
            // Backlog line 140: timeout/tags/auth/redirects/compression packed
            // into ONE JSON string — the native bridge closure is arity-
            // capped (rquickjs Func supports ctx + 6 script args), so the
            // per-request params share a single argument. The legacy PM
            // fallback below keeps its own contract (extra params unsupported
            // there).
            JSON.stringify({
                timeoutMs: canonical.timeoutMs,
                tags: canonical.tags,
                auth: canonical.auth,
                redirects: canonical.redirects,
                compression: canonical.compression,
                // Backlog line 150: binary request bodies ride as base64.
                bodyB64: canonical.bodyB64
            })
        );
    } else if (typeof __tropel_pm_send_request === 'function') {
        resultJson = __tropel_pm_send_request(
            canonical.method,
            canonical.url,
            headersJson,
            canonical.body,
            canonical.timeoutMs,
            canonical.responseType
        );
    } else {
        throw new Error(
            'k6 http.* requires a native HTTP bridge — neither __tropel_k6_http_request ' +
            'nor __tropel_pm_send_request is available. Check that the K6Driver or PM bridge ' +
            'was properly installed.'
        );
    }

    var result;
    if (typeof resultJson === 'string') {
        // Legacy path: the PM fallback bridge returns a JSON string.
        try {
            result = JSON.parse(resultJson);
        } catch (e) {
            throw new Error('k6 http.request: failed to parse native response: ' + e.message);
        }
    } else {
        // Native k6 bridge returns a live JS object — no JSON round trip
        // (avoids the old 3-4 full-body copies per response).
        result = resultJson;
    }

    var respCode = result.code || result.status_code || result.status || 0;
    var respBody = result.body || '';
    var respHeaders = result.headers || {};
    var respTime = result.responseTime || result.response_time || 0;

    // Backlog line 150: batch responses with binary bodies carry base64 +
    // body_b64 (JSON can't hold raw bytes) — decode to an ArrayBuffer so
    // res.body instanceof ArrayBuffer behaves like the native single-request
    // bridge (which sets a real ArrayBuffer directly).
    if (result.body_b64 && typeof respBody === 'string' && respBody !== '') {
        respBody = base64ToBytes(respBody).buffer;
    }

    // Normalize headers from {key: value} or array format into a fresh
    // object. Keys are kept EXACTLY as the native bridge delivered them (Go
    // MIME canonical form: Content-Type, X-Request-Id) — the old
    // toLowerCase() here made every k6 doc idiom `res.headers['Content-Type']`
    // return undefined (backlog line 139). The copy protects K6Response from
    // sharing the bridge's object (user mutation of res.headers must not leak
    // back into the native response).
    var normalizedHeaders = {};
    if (Array.isArray(respHeaders)) {
        for (var hi = 0; hi < respHeaders.length; hi++) {
            var h = respHeaders[hi];
            if (h && h.key) {
                normalizedHeaders[h.key] = h.value !== undefined ? h.value : '';
            }
        }
    } else if (typeof respHeaders === 'object') {
        for (var hk in respHeaders) {
            if (respHeaders.hasOwnProperty(hk)) {
                normalizedHeaders[hk] = respHeaders[hk];
            }
        }
    }

    // Backlog line 150: prefer the REAL connection-phase timings the native
    // bridge now delivers; fall back to the old duration-only approximation
    // when a legacy/PM bridge returns no timings object.
    var timings = result.timings || {
        blocked: 0,
        connecting: 0,
        tls_handshaking: 0,
        sending: 0,
        waiting: respTime,
        receiving: 0,
        duration: respTime
    };
    // Ensure every k6 key exists (native timings include Tropel's extra dns).
    if (typeof timings === 'object') {
        timings = {
            blocked: timings.blocked || 0,
            connecting: timings.connecting || 0,
            tls_handshaking: timings.tls_handshaking || 0,
            sending: timings.sending || 0,
            waiting: timings.waiting || respTime,
            receiving: timings.receiving || 0,
            duration: timings.duration || respTime
        };
    }

    // Backlog line 102: cookies from the native bridge (k6 shape) and the
    // request that produced this response. k6's Response.request is the
    // REQUEST (method/url/headers/body/cookies) — not the response — so
    // headers come from canonical.headers and body from canonical.body.
    var resp = new K6Response(
        respCode, respBody, normalizedHeaders, timings, url,
        result.cookies, {
            method: canonical.method,
            url: canonical.url,
            headers: canonical.headers,
            body: canonical.body,
            cookies: result.cookies || {}
        }
    );
    // Backlog line 150: k6 Response.error ("" on success) / error_code (0 on
    // success, 1xxx on transport failure) — `if (res.error)` now detects
    // failures like k6.
    resp.error = result.error || '';
    resp.error_code = result.error_code || 0;
    return resp;
}

function normalizeK6Request(method, url, body, params) {
    method = (method || 'GET').toUpperCase();
    params = params || {};

    // COPY the caller's headers: serializeK6Body stamps Content-Type for
    // object bodies (and the generated boundary for multipart), and real k6
    // scripts hoist `params` to module scope — writing on the caller's
    // object leaked iteration 1's Content-Type into every later iteration
    // (a string body posted on iteration 2 was still labelled
    // application/json). The copy keeps the stamp per-request.
    var headers = {};
    var srcHeaders = params.headers;
    if (srcHeaders && typeof srcHeaders === 'object') {
        if (Array.isArray(srcHeaders)) {
            // Postman array form: [{key: 'Authorization', value: 'Bearer T'}]
            for (var i = 0; i < srcHeaders.length; i++) {
                var h = srcHeaders[i];
                if (h && h.key) headers[h.key] = h.value !== undefined ? h.value : '';
            }
        } else {
            for (var hk in srcHeaders) {
                if (srcHeaders.hasOwnProperty(hk)) headers[hk] = srcHeaders[hk];
            }
        }
    }
    // Backlog P1: the GLOBAL HttpConfig.request_timeout must bound k6
    // requests too (it is the client-level ceiling, applied via reqwest's
    // `.timeout()` at build time). Before this fix the shim hardcoded
    // `params.timeout || '30s'` and packed timeoutMs=30000 into extras, so
    // the driver set `request.timeout = Some(30s)` and `execute()`'s
    // `req_builder.timeout(30s)` OVERRODE the global 500ms — the global
    // config was dead on the k6 path. Now timeoutMs is only packed when the
    // script EXPLICITLY sets params.timeout; absent, it stays 0 and the
    // driver leaves request.timeout None so the client-level global applies
    // (falling back to the engine's default, like Postman requests).
    var timeoutMs = 0;
    if (params.timeout !== undefined && params.timeout !== null && params.timeout !== '') {
        var timeout = params.timeout;
        if (typeof timeout === 'string') {
            var match = timeout.match(/^(\d+)(ms|s|m)?$/);
            if (match) {
                var val = parseInt(match[1], 10);
                var unit = match[2] || 'ms';
                if (unit === 's') timeoutMs = val * 1000;
                else if (unit === 'm') timeoutMs = val * 60000;
                else timeoutMs = val;
            }
        } else if (typeof timeout === 'number') {
            timeoutMs = timeout;
        }
    }
    // k6 params.responseType: "text" (default) | "binary" | "none"
    var responseType = params.responseType || 'text';

    // k6 params.cookies: {name: value} — merged into the Cookie header
    // (k6 sends cookies as a Cookie header), combined with any explicit
    // Cookie header the script set. `headers` is already a per-request copy,
    // so this can't leak into the caller's module-scope params.
    var cookies = params.cookies;
    if (cookies && typeof cookies === 'object') {
        var cookieParts = [];
        for (var ck in cookies) {
            if (cookies.hasOwnProperty(ck) && cookies[ck] !== undefined && cookies[ck] !== null) {
                cookieParts.push(encodeURIComponent(ck) + '=' + encodeURIComponent(String(cookies[ck])));
            }
        }
        if (cookieParts.length > 0) {
            var existingCookie = headers['Cookie'] || headers['cookie'] || '';
            headers['Cookie'] = existingCookie
                ? existingCookie + '; ' + cookieParts.join('; ')
                : cookieParts.join('; ');
        }
    }

    var serialized = serializeK6Body(body, headers);
    return {
        method: method,
        url: url,
        headers: serialized.headers,
        body: serialized.body,
        bodyB64: serialized.binary,
        timeoutMs: timeoutMs,
        responseType: responseType,
        // Backlog line 140: tags/auth/redirects/compression were silently
        // dropped and timeout was parsed then discarded. The native bridge
        // now receives all of them (auth translated to the tagged AuthConfig
        // form the Rust side deserializes).
        tags: params.tags || {},
        auth: toAuthConfig(params.auth),
        redirects: params.redirects !== undefined ? params.redirects : -1,
        compression: params.compression || '',
    };
}

// Translate k6's `params.auth` object (no type discriminator) into the
// tagged AuthConfig form the native bridge deserializes. k6 infers the type
// from which fields are present: token → bearer, access_token → oauth2,
// access_key → aws-sigv4, username → basic (k6's documented shapes).
function toAuthConfig(auth) {
    if (!auth || typeof auth !== 'object') return null;
    if (auth.token !== undefined) {
        return { type: 'bearer', token: String(auth.token) };
    }
    if (auth.access_token !== undefined) {
        return {
            type: 'oauth2',
            access_token: String(auth.access_token),
            token_type: auth.token_type !== undefined ? String(auth.token_type) : null,
        };
    }
    if (auth.access_key !== undefined) {
        return {
            type: 'aws-sigv4',
            access_key: String(auth.access_key),
            secret_key: auth.secret_key !== undefined ? String(auth.secret_key) : '',
            region: auth.region !== undefined ? String(auth.region) : null,
            service: auth.service !== undefined ? String(auth.service) : null,
            session_token: auth.session_token !== undefined ? String(auth.session_token) : null,
        };
    }
    if (auth.username !== undefined) {
        return {
            type: 'basic',
            username: String(auth.username),
            password: auth.password !== undefined ? String(auth.password) : '',
        };
    }
    return null;
}

// Backlog line 150: http.file() — k6's binary-file body factory. Returns a
// K6File with `data` (string | ArrayBuffer | Uint8Array), `filename`, and
// `content_type`. Used directly as a body or inside a multipart object; the
// bytes are base64-encoded for the native bridge (which decodes to raw).
function K6File(data, filename, contentType) {
    this.data = data;
    this.filename = filename;
    this.content_type = contentType;
}

function k6FileToBytes(file) {
    var d = file.data;
    if (typeof d === 'string') {
        return { base64: false, text: d };
    }
    if (typeof Uint8Array !== 'undefined' && d instanceof Uint8Array) {
        return { base64: true, bytes: d };
    }
    if (d instanceof ArrayBuffer) {
        return { base64: true, bytes: new Uint8Array(d) };
    }
    return { base64: false, text: String(d) };
}

function bytesToBase64(bytes) {
    var out = '';
    for (var i = 0; i < bytes.length; i += 3) {
        var b0 = bytes[i];
        var b1 = i + 1 < bytes.length ? bytes[i + 1] : 0;
        var b2 = i + 2 < bytes.length ? bytes[i + 2] : 0;
        var n = (b0 << 16) | (b1 << 8) | b2;
        out += 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'[n >> 18 & 63]
            + 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'[n >> 12 & 63]
            + 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'[n >> 6 & 63]
            + 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'[n & 63];
    }
    var pad = bytes.length % 3;
    if (pad === 1) out = out.slice(0, -2) + '==';
    else if (pad === 2) out = out.slice(0, -1) + '=';
    return out;
}

function base64ToBytes(b64) {
    var alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
    var out = new Uint8Array(Math.floor(b64.length * 3 / 4));
    var o = 0, buffer = 0, bits = 0;
    for (var i = 0; i < b64.length; i++) {
        var c = b64[i];
        if (c === '=') break;
        var idx = alphabet.indexOf(c);
        if (idx < 0) continue;
        buffer = (buffer << 6) | idx;
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            out[o++] = (buffer >> bits) & 255;
        }
    }
    return out.subarray(0, o);
}

function serializeK6Body(body, headers) {
    var bodyStr = '';
    var bodyB64 = false;
    if (body !== null && body !== undefined) {
        if (typeof body === 'string') {
            bodyStr = body;
        } else if (body instanceof K6File) {
            var fb = k6FileToBytes(body);
            if (fb.base64) {
                bodyStr = bytesToBase64(fb.bytes);
                bodyB64 = true;
            } else {
                bodyStr = fb.text;
            }
            // k6 stamps the file's content type on the request when the body
            // IS the file.
            if (body.content_type && !headers['Content-Type'] && !headers['content-type']) {
                headers['Content-Type'] = body.content_type;
            }
        } else if (body instanceof ArrayBuffer) {
            bodyStr = bytesToBase64(new Uint8Array(body));
            bodyB64 = true;
        } else if (typeof Uint8Array !== 'undefined' && body instanceof Uint8Array) {
            bodyStr = bytesToBase64(body);
            bodyB64 = true;
        } else {
            var contentType = headers['Content-Type'] || headers['content-type'];
            if (contentType && contentType.indexOf('multipart/form-data') !== -1 && typeof body === 'object') {
                var multipart = buildMultipartFormData(body);
                bodyStr = multipart.body;
                bodyB64 = multipart.binary;
                // ALWAYS stamp the full generated content-type. The old
                // `!headers['Content-Type']` guard was false exactly when the
                // user declared multipart/form-data (they set the type but not
                // a boundary), so the generated boundary never reached the
                // header and every multipart request was unparseable. The body
                // was framed with OUR boundary, so the header must advertise
                // exactly that boundary. `headers` is a per-request copy
                // (normalizeK6Request clones params.headers), so this can't
                // leak into the caller's object. Drop any user-declared
                // lowercase variant — leaving it would send TWO Content-Type
                // headers (one boundary-less) and the Rust side's case-
                // sensitive HashMap would keep both.
                delete headers['content-type'];
                headers['Content-Type'] = multipart.contentType;
            } else if (contentType && contentType.indexOf('application/x-www-form-urlencoded') !== -1 && typeof body === 'object') {
                bodyStr = serializeUrlEncoded(body);
            } else if (!contentType && typeof body === 'object') {
                // k6 default: objects are form-urlencoded, not JSON.
                bodyStr = serializeUrlEncoded(body);
                if (!headers['Content-Type'] && !headers['content-type']) {
                    headers['Content-Type'] = 'application/x-www-form-urlencoded';
                }
            } else {
                try {
                    bodyStr = JSON.stringify(body);
                    if (!headers['Content-Type'] && !headers['content-type']) {
                        headers['Content-Type'] = 'application/json';
                    }
                } catch (e) {
                    bodyStr = String(body);
                }
            }
        }
    }

    return { body: bodyStr, headers: headers, binary: bodyB64 };
}

function buildMultipartFormData(object) {
    var boundary = '----TropelFormBoundary' + Math.random().toString(36).slice(2);
    var body = '';
    var binary = false;

    for (var key in object) {
        if (!object.hasOwnProperty(key)) continue;
        var value = object[key];
        if (value === undefined || value === null) {
            value = '';
        } else if (value instanceof K6File) {
            // Backlog line 150: file parts carry a filename + content-type and
            // the raw bytes (base64 for the native bridge, which decodes).
            var fb = k6FileToBytes(value);
            var partData = fb.base64 ? bytesToBase64(fb.bytes) : fb.text;
            binary = binary || fb.base64;
            body += '--' + boundary + '\r\n';
            body += 'Content-Disposition: form-data; name="' + escapeMultipartFieldName(key)
                + '"; filename="' + escapeMultipartFieldName(value.filename || 'file') + '"\r\n';
            if (value.content_type) {
                body += 'Content-Type: ' + value.content_type + '\r\n';
            }
            body += '\r\n' + partData + '\r\n';
            continue;
        } else if (typeof value !== 'string') {
            try {
                value = JSON.stringify(value);
            } catch (e) {
                value = String(value);
            }
        }
        body += '--' + boundary + '\r\n';
        body += 'Content-Disposition: form-data; name="' + escapeMultipartFieldName(key) + '"\r\n\r\n';
        body += value + '\r\n';
    }
    body += '--' + boundary + '--\r\n';

    return { body: body, contentType: 'multipart/form-data; boundary=' + boundary, binary: binary };
}

function serializeUrlEncoded(object) {
    var parts = [];
    for (var key in object) {
        if (!object.hasOwnProperty(key)) continue;
        var value = object[key];
        if (value === undefined || value === null) {
            value = '';
        }
        parts.push(encodeURIComponent(key) + '=' + encodeURIComponent(String(value)));
    }
    return parts.join('&');
}

function escapeMultipartFieldName(name) {
    return String(name).replace(/\\/g, '\\\\').replace(/"/g, '\\"');
}

// ══════════════════════════════════════════════════════════════════
// Response object (k6-compatible)
// ══════════════════════════════════════════════════════════════════

// Backlog line 102: res.cookies / res.request / proto / remote_ip / html()
// were absent — res.cookies['sid'] threw TypeError. Cookies are real data
// from the native bridge (name -> [cookie objects], k6 shape); request is
// the k6 Response.request {method, url, headers, body, cookies}; proto /
// remote_ip are best-effort defaults (reqwest doesn't expose the wire
// version or peer IP through the SDK Response), present so scripts never
// throw on them.
function K6Response(status, body, headers, timings, url, cookies, requestInfo) {
    this.status = status;
    this.body = body;
    this.headers = headers || {};
    this.timings = timings || { blocked: 0, connecting: 0, tls_handshaking: 0, sending: 0, waiting: 0, receiving: 0, duration: 0 };
    this.url = url || '';
    this.status_text = String(status) + ' ' + getStatusText(status);
    this.cookies = cookies || {};
    this.request = requestInfo || { method: 'GET', url: url || '', headers: this.headers, body: body, cookies: this.cookies };
    this.proto = 'HTTP/1.1';
    this.remote_ip = '';
}

// Backlog line 102: res.html() was absent entirely. k6 parses the body
// into a jQuery-like Selection (goquery); quickjs has no DOM parser, so
// this is a regex-based subset covering the common idioms — find by tag /
// #id / .class, plus html()/text()/attr()/eq()/size(). Complex or unknown
// selectors yield an EMPTY selection (never throw), matching k6's
// behaviour for a no-match.
function K6HtmlSelection(body) {
    this._body = String(body || '');
    this._els = [this._body]; // one "element": the whole document
    this.length = 1;
}
K6HtmlSelection.prototype.find = function (selector) {
    var s = String(selector || '').trim();
    var matches = [];
    // Capture the WHOLE element (open tag + content + matching close tag, or
    // a self-closing tag), so .text() returns the element's text exactly like
    // goquery's find() — matching only the open tag would strip it to ''.
    var re = null;
    if (/^#/.test(s)) {
        var id = s.slice(1);
        re = new RegExp(
            '<([a-z][a-z0-9]*)[^>]*\\sid=["\']' + id + '["\'][^>]*>[\\s\\S]*?<\\/\\1>'
            + '|<[a-z][a-z0-9]*[^>]*\\sid=["\']' + id + '["\'][^>]*\\/>',
            'gi'
        );
    } else if (/^\./.test(s)) {
        var cls = s.slice(1);
        re = new RegExp(
            '<([a-z][a-z0-9]*)[^>]*\\sclass=["\'][^"\']*\\b' + cls + '\\b[^"\']*["\'][^>]*>[\\s\\S]*?<\\/\\1>'
            + '|<[a-z][a-z0-9]*[^>]*\\sclass=["\'][^"\']*\\b' + cls + '\\b[^"\']*["\'][^>]*\\/>',
            'gi'
        );
    } else if (/^[a-z][a-z0-9]*$/.test(s)) {
        re = new RegExp(
            '<(' + s + ')\\b[^>]*>[\\s\\S]*?<\\/\\1>|<' + s + '\\b[^>]*\\/>',
            'gi'
        );
    }
    if (re) {
        var m;
        while ((m = re.exec(this._body)) !== null) matches.push(m[0]);
    }
    var out = new K6HtmlSelection('');
    out._els = matches;
    out.length = matches.length;
    return out;
};
K6HtmlSelection.prototype.html = function () {
    return this._els.join('');
};
K6HtmlSelection.prototype.text = function () {
    return this._els.map(function (e) { return String(e).replace(/<[^>]*>/g, ''); }).join('');
};
K6HtmlSelection.prototype.attr = function (name) {
    if (!this._els.length) return undefined;
    var re = new RegExp('\\s' + name + '=["\']([^"\']*)["\']', 'i');
    var m = String(this._els[0]).match(re);
    return m ? m[1] : undefined;
};
K6HtmlSelection.prototype.eq = function (i) {
    var out = new K6HtmlSelection('');
    out._els = [this._els[i]].filter(function (e) { return e !== undefined; });
    out.length = out._els.length;
    return out;
};
K6HtmlSelection.prototype.size = function () {
    return this.length;
};
K6Response.prototype.html = function () {
    // Only text bodies carry parseable HTML; binary/none responseTypes
    // yield an empty selection rather than garbage from coercion.
    if (typeof this.body === 'string') {
        return new K6HtmlSelection(this.body);
    }
    return new K6HtmlSelection('');
};

// Backlog line 154: k6's res.json() accepts an OPTIONAL selector. k6 uses
// gjson-style dotted paths ('a.b.0' -> obj.a.b[0], also array wildcards like
// 'items.#.name' and '#.id'). This is a JSONPath-lite subset covering the
// common cases: dotted keys, numeric array indices, and the k6 array
// wildcard '#' (matches every element of an array). Unknown paths return
// undefined (k6 parity — no throw).
function resolveJsonSelector(value, selector) {
    if (!selector) {
        return value;
    }
    var parts = String(selector).split('.');
    for (var i = 0; i < parts.length; i++) {
        var part = parts[i];
        if (part === '') {
            continue; // tolerate leading/trailing/double dots
        }
        if (part === '#') {
            // Array wildcard: expand to an array of each element's remainder.
            if (!Array.isArray(value)) {
                return undefined;
            }
            var rest = parts.slice(i + 1).join('.');
            var out = [];
            for (var j = 0; j < value.length; j++) {
                out.push(resolveJsonSelector(value[j], rest));
            }
            return out;
        }
        if (value === null || value === undefined) {
            return undefined;
        }
        if (Array.isArray(value)) {
            var idx = Number(part);
            if (!isFinite(idx) || idx < 0 || idx >= value.length) {
                return undefined;
            }
            value = value[idx];
        } else if (typeof value === 'object' && Object.prototype.hasOwnProperty.call(value, part)) {
            value = value[part];
        } else {
            return undefined;
        }
    }
    return value;
}

K6Response.prototype.json = function (selector) {
    if (!this.body || this.body === '') {
        throw new Error('Response body is empty — cannot parse JSON');
    }
    // Parse fresh on every call, exactly like k6: a script that mutates the
    // returned object must see a clean re-parse on the next .json() call
    // (k6 does not cache, and caching would persist user mutations). The
    // heavy copy savings for the response envelope itself come from the
    // native-object bridge (driver.rs), not from re-parsing here.
    var parsed = JSON.parse(this.body);
    return resolveJsonSelector(parsed, selector);
};

// ══════════════════════════════════════════════════════════════════
// http.* methods
// ══════════════════════════════════════════════════════════════════

var http = {};

http.get = function (url, params) { return k6HTTPRequest('GET', url, null, params); };
http.post = function (url, body, params) { return k6HTTPRequest('POST', url, body, params); };
http.put = function (url, body, params) { return k6HTTPRequest('PUT', url, body, params); };
http.del = function (url, params) { return k6HTTPRequest('DELETE', url, null, params); };
http.delete = function (url, params) { return k6HTTPRequest('DELETE', url, null, params); };
http.patch = function (url, body, params) { return k6HTTPRequest('PATCH', url, body, params); };

// Backlog line 150: k6's http.file(data, filename, contentType) → K6File.
// The bytes survive the String-typed bridge as base64 (decoded to raw on the
// Rust side), so binary uploads are finally possible.
http.file = function (data, filename, contentType) {
    return new K6File(data, filename, contentType);
};
http.head = function (url, params) { return k6HTTPRequest('HEAD', url, null, params); };
http.options = function (url, params) { return k6HTTPRequest('OPTIONS', url, null, params); };
http.request = function (method, url, body, params) { return k6HTTPRequest(method, url, body, params); };

// http.batch — sequential execution (QuickJS has no async)
//
// k6 semantics (backlog line 137): an ARRAY of requests returns an ARRAY of
// responses (index-preserving — Array.isArray true, .forEach/spread/
// destructuring work); an OBJECT of named requests returns an OBJECT keyed
// by name. The old implementation always returned a keyed object, so the
// documented batch idiom (`res[0].status`) silently broke.
http.batch = function (requests) {
    // Backlog §3: `http.batch('abc')` used to fall through both branches
    // (not an array, typeof !== 'object') and return {} silently.
    if (!requests || typeof requests !== 'object') {
        throw new Error('http.batch requires an array or object of requests');
    }

    var isArrayInput = Array.isArray(requests);
    var entries = [];
    var keys = [];

    if (isArrayInput) {
        for (var bi = 0; bi < requests.length; bi++) {
            var e = normalizeBatchEntry(requests[bi], bi);
            entries.push(e);
            // W2 line 169: array input keys by POSITION — the caller's key.
            // k6 returns responses in order; req.name must never hijack the
            // response key (that collision lost real responses to the map).
            keys.push(bi);
        }
    } else if (typeof requests === 'object') {
        var names = Object.keys(requests);
        for (var ni = 0; ni < names.length; ni++) {
            var e = normalizeBatchEntry(requests[names[ni]], names[ni]);
            entries.push(e);
            // W2 line 169: object input keys by the CALLER's property name —
            // res.front must resolve even when the request carries a `name`.
            keys.push(names[ni]);
        }
    }

    var results = isArrayInput ? [] : {};

    if (typeof __tropel_k6_http_batch === 'function') {
        // W2 line 169: batch keys are the CALLER's keys — array input →
        // positional index, object input → property name — so `res.front`
        // resolves even when a request carries a `name` (the old
        // e.key/req.name fallback hijacked the response key, and colliding
        // names lost real responses to the driver's last-write-wins map).
        // Keys are unique by construction, so the name-based dedupe loop is
        // gone entirely.
        var finalKeys = [];
        for (var dki = 0; dki < keys.length; dki++) {
            finalKeys.push(String(keys[dki]));
        }
        var normalized = [];
        for (var ei = 0; ei < entries.length; ei++) {
            var entry = entries[ei];
            var canonical = normalizeK6Request(entry.method, entry.url, entry.body, entry.params);
            normalized.push({
                key: String(finalKeys[ei]),
                method: canonical.method,
                url: canonical.url,
                headers_json: JSON.stringify(canonical.headers),
                body: canonical.body,
                response_type: canonical.responseType,
                // W2 line 169: ONE canonical extras wire shape shared with
                // the single-request bridge (timeoutMs/tags/auth/redirects/
                // compression/bodyB64) — the old timeout_ms/tags_json/
                // auth_json/body_b64 variants diverged on four of seven
                // fields and dropped the whole tag map on non-string values.
                extras: JSON.stringify({
                    timeoutMs: canonical.timeoutMs,
                    tags: canonical.tags,
                    auth: canonical.auth,
                    redirects: canonical.redirects,
                    compression: canonical.compression,
                    bodyB64: canonical.bodyB64
                })
            });
        }

        // W2 line 169: the native bridge returns a LIVE object (the
        // escaped-JSON round trip is gone); legacy stubs / the PM fallback
        // still return a JSON string.
        var batchResult = __tropel_k6_http_batch(JSON.stringify(normalized));
        if (typeof batchResult === 'string') {
            batchResult = JSON.parse(batchResult);
        }
        for (var ei = 0; ei < entries.length; ei++) {
            var entry = entries[ei];
            var key = String(finalKeys[ei]);
            // Backlog §3: a missing key used to fabricate {status:0, body:'',
            // error:''} — and error==='' made `if (res.error)` miss it. The
            // driver inserts every sent key (incl. error envelopes), so a
            // missing one is a contract violation — fail loudly.
            // W2 line 169: `in` walks the prototype chain — a caller key named
            // 'constructor'/'toString' would pass the guard and read the
            // inherited function instead of a response. Own-property check.
            if (!Object.prototype.hasOwnProperty.call(batchResult, key)) {
                throw new Error('http.batch: native bridge returned no response for key "' + key + '"');
            }
            // Wrap each entry as a K6Response so `.json()`, `.status`, `.body`
            // behave like the sequential path (k6 returns Response objects).
            var raw = batchResult[key];
            var headers = raw.headers || {};
            var normalizedHeaders = {};
            if (Array.isArray(headers)) {
                for (var hi = 0; hi < headers.length; hi++) {
                    var h = headers[hi];
                    if (h && h.key) {
                        normalizedHeaders[h.key] = h.value !== undefined ? h.value : '';
                    }
                }
            } else {
                for (var hk in headers) {
                    if (headers.hasOwnProperty(hk)) {
                        normalizedHeaders[hk] = headers[hk];
                    }
                }
            }
            var code = raw.code || raw.status_code || raw.status || 0;
            var rtime = raw.responseTime || raw.response_time || 0;
            // Backlog line 150: batch entries carry real timings + error +
            // error_code + (binary) body_b64 — mirror the single path.
            var bodyVal = raw.body || '';
            if (raw.body_b64 && typeof bodyVal === 'string' && bodyVal !== '') {
                bodyVal = base64ToBytes(bodyVal).buffer;
            }
            var timings = raw.timings || {
                blocked: 0,
                connecting: 0,
                tls_handshaking: 0,
                sending: 0,
                waiting: rtime,
                receiving: 0,
                duration: rtime
            };
            if (typeof timings === 'object') {
                timings = {
                    blocked: timings.blocked || 0,
                    connecting: timings.connecting || 0,
                    tls_handshaking: timings.tls_handshaking || 0,
                    sending: timings.sending || 0,
                    waiting: timings.waiting || rtime,
                    receiving: timings.receiving || 0,
                    duration: timings.duration || rtime
                };
            }
            var resp = new K6Response(
                code, bodyVal, normalizedHeaders, timings, entry.url,
                raw.cookies, {
                    // k6 parity: Response.request is the REQUEST, so headers
                    // are the serialized request headers from the first loop
                    // (the response headers live on res.headers, not here).
                    method: entry.method || 'GET',
                    url: entry.url || '',
                    headers: normalized[ei] ? JSON.parse(normalized[ei].headers_json) : {},
                    body: entry.body !== undefined ? entry.body : '',
                    cookies: raw.cookies || {}
                }
            );
            resp.error = raw.error || '';
            resp.error_code = raw.error_code || 0;
            if (isArrayInput) {
                results.push(resp);
            } else {
                results[key] = resp;
            }
        }
    } else {
        for (var ei = 0; ei < entries.length; ei++) {
            var entry = entries[ei];
            var resp = k6HTTPRequest(entry.method, entry.url, entry.body, entry.params);
            if (isArrayInput) {
                results.push(resp);
            } else {
                results[String(keys[ei])] = resp;
            }
        }
    }

    return results;
};

function normalizeBatchEntry(req, defaultKey) {
    if (typeof req === 'string') {
        return { key: defaultKey, method: 'GET', url: req, body: null, params: {} };
    }
    if (Array.isArray(req) && req.length >= 2) {
        return {
            key: defaultKey,
            method: req[0],
            url: req[1],
            body: req.length > 2 ? req[2] : null,
            params: req.length > 3 ? req[3] : {}
        };
    }
    if (typeof req === 'object') {
        // Preserve the object-form entry's responseType (k6: params.responseType)
        var entryParams = req.params || {};
        return {
            key: req.name != null ? req.name : defaultKey,
            method: req.method || 'GET',
            url: req.url || '',
            // Backlog §3: `req.body || null` dropped 0/''/false — only
            // undefined/null mean "no body".
            body: req.body !== undefined ? req.body : null,
            params: {
                headers: req.headers || entryParams.headers || {},
                tags: req.tags || entryParams.tags || {},
                // Backlog §3: object-form entries forced `timeout:'30s'`,
                // overriding the global HttpConfig.request_timeout. Mirror
                // the single path — leave it unset so timeoutMs stays 0 and
                // the driver applies the client-level global.
                timeout: req.timeout || entryParams.timeout,
                responseType: entryParams.responseType || req.responseType || 'text',
                // Backlog §3: object-form entries dropped auth/redirects/
                // compression/cookies that the single path honors.
                auth: req.auth !== undefined ? req.auth : entryParams.auth,
                redirects: req.redirects !== undefined ? req.redirects : entryParams.redirects,
                compression: req.compression !== undefined ? req.compression : entryParams.compression,
                cookies: req.cookies !== undefined ? req.cookies : entryParams.cookies
            }
        };
    }
    throw new Error('Invalid batch request entry: ' + JSON.stringify(req));
}

// ══════════════════════════════════════════════════════════════════
// ws.* — k6/ws parity (event-driven WebSocket)
// ══════════════════════════════════════════════════════════════════
//
// Delegates to native bridges registered by the K6Driver:
//   __tropel_k6_ws_connect(url, headers_json) -> {id, error}
//   __tropel_k6_ws_step(id, timeout_ms)          -> {type, ...}
//   __tropel_k6_ws_send(id, data) / _ping(id) / _close(id, code, reason)
//   __tropel_k6_ws_finish(id)
//
// QuickJS has no async event loop, so ws.connect() runs a synchronous
// event pump: the callback registers handlers, then the pump calls
// __tropel_k6_ws_step() to block for the next event (open/message/close/
// error/ping/pong) and dispatches to the registered socket.on() handlers.
// The pump ends when the socket closes (server close, socket.close(), or an
// error). This mirrors k6's semantics within one iteration: the VU stays on
// the socket until it closes.

var ws = {};

function K6Socket(sessionId) {
    this._id = sessionId;
    this._handlers = {};
    this._timers = [];
    this._closed = false;
}

K6Socket.prototype.on = function (event, handler) {
    if (typeof handler !== 'function') {
        throw new Error('socket.on(event, handler) requires a function handler');
    }
    var list = this._handlers[event] || (this._handlers[event] = []);
    list.push(handler);
    return this;
};

K6Socket.prototype._emit = function (event, arg1, arg2) {
    var list = this._handlers[event];
    if (!list) {
        return;
    }
    for (var i = 0; i < list.length; i++) {
        try {
            list[i].call(this, arg1, arg2);
        } catch (e) {
            fail('ws handler "' + event + '" threw: ' + e);
        }
    }
};

K6Socket.prototype.send = function (data) {
    if (typeof __tropel_k6_ws_send !== 'function') {
        throw new Error('ws.send requires the native ws bridge (__tropel_k6_ws_send)');
    }
    __tropel_k6_ws_send(this._id, String(data));
    return this;
};

K6Socket.prototype.ping = function () {
    if (typeof __tropel_k6_ws_ping !== 'function') {
        throw new Error('ws.ping requires the native ws bridge (__tropel_k6_ws_ping)');
    }
    __tropel_k6_ws_ping(this._id);
    return this;
};

K6Socket.prototype.close = function (code, reason) {
    if (typeof __tropel_k6_ws_close !== 'function') {
        throw new Error('ws.close requires the native ws bridge (__tropel_k6_ws_close)');
    }
    var closeCode = code || 1000;
    var closeReason = reason || '';
    __tropel_k6_ws_close(this._id, closeCode, closeReason);
    // Backlog line 148: a LOCAL close() must still dispatch the 'close'
    // handler. The old code only set _closed, so the synchronous pump
    // (while !settled && !socket._closed) exited at the next iteration and
    // `socket.on('close', ...)` never fired — the k6 idiom of calling
    // socket.close() inside on('open')/'message' leaked the final cleanup
    // callback. Guarded so a server-close that already fired the handler
    // can't double-dispatch.
    if (!this._closed) {
        this._closed = true;
        this._emit('close', closeCode, closeReason);
    }
    return this;
};

K6Socket.prototype.setTimeout = function (fn, ms) {
    this._timers.push({ fn: fn, ms: ms, at: Date.now() + ms, interval: false });
    return this;
};

K6Socket.prototype.setInterval = function (fn, ms) {
    this._timers.push({ fn: fn, ms: ms, at: Date.now() + ms, interval: true });
    return this;
};

// Fire due timers. One-shot timeouts are removed; intervals are rescheduled.
K6Socket.prototype._runTimers = function () {
    var now = Date.now();
    var keep = [];
    for (var i = 0; i < this._timers.length; i++) {
        var t = this._timers[i];
        if (now >= t.at) {
            try {
                t.fn();
            } catch (e) {
                fail('ws timer threw: ' + e);
            }
            if (t.interval) {
                t.at = now + t.ms;
                keep.push(t);
            }
        } else {
            keep.push(t);
        }
    }
    this._timers = keep;
};

ws.connect = function (url, params, callback) {
    params = params || {};
    var headers = params.headers || {};
    if (typeof __tropel_k6_ws_connect !== 'function') {
        throw new Error(
            'ws.connect requires the native ws bridge (__tropel_k6_ws_connect) — ' +
            'check that the K6Driver installed the ws bridges'
        );
    }
    var connectRes = JSON.parse(__tropel_k6_ws_connect(url, JSON.stringify(headers)));
    if (connectRes.error) {
        throw new Error('ws.connect failed: ' + connectRes.error);
    }
    var socket = new K6Socket(connectRes.id);
    // Safety cap: if the peer never closes and the script never calls
    // close(), end the session after params.timeout (ms) so the synchronous
    // pump can never hang the VU. Default 5 minutes.
    var maxSessionMs = (params.timeout > 0) ? params.timeout : 300000;
    var sessionStart = Date.now();
    var settled = false;
    try {
        if (typeof callback === 'function') {
            callback(socket);
        }
        // Synchronous event pump: drive the socket until it closes.
        while (!settled && !socket._closed) {
            if (Date.now() - sessionStart > maxSessionMs) {
                socket.close(1000, 'session timeout');
                settled = true;
                break;
            }
            socket._runTimers();
            var evt = JSON.parse(__tropel_k6_ws_step(connectRes.id, 50));
            if (evt.type === 'open') {
                socket._emit('open');
            } else if (evt.type === 'message') {
                socket._emit('message', evt.data);
            } else if (evt.type === 'ping') {
                socket._emit('ping');
            } else if (evt.type === 'pong') {
                socket._emit('pong');
            } else if (evt.type === 'close') {
                // Mark closed BEFORE dispatching so a defensive
                // socket.close() inside the close handler cannot
                // double-dispatch (backlog line 148: _closed is the
                // authoritative flag for both local and remote closes).
                socket._closed = true;
                socket._emit('close', evt.code, evt.reason);
                settled = true;
            } else if (evt.type === 'error') {
                // Same guard: once errored, a later local close() must not
                // fire 'close' a second time.
                socket._closed = true;
                socket._emit('error', evt.message);
                settled = true;
            }
            // {type:'none'} — step timed out with no event; loop again
            // (timers may fire, or close may arrive on a later step).
        }
    } finally {
        // ALWAYS tear down the native session — even when the user callback
        // or a socket.on handler threw — so the registry entry and its
        // background socket task are not leaked.
        if (typeof __tropel_k6_ws_finish === 'function') {
            try {
                __tropel_k6_ws_finish(connectRes.id);
            } catch (e) { /* teardown must not mask the original error */ }
        }
    }
    return socket;
};

// ══════════════════════════════════════════════════════════════════
// Global functions
// ══════════════════════════════════════════════════════════════════

// fail(msg) — throws an error that aborts the current iteration
function fail(msg) {
    throw new Error('k6 fail: ' + (msg || 'test failed'));
}

// check(val, conds) — defined in pm-api/pm.js if loaded, else here.
// NOTE: uses `var` assignment (NOT `function` inside the guard) — using `var`
// avoids re-declaring the function when the guard condition is true.
if (typeof check !== 'function') {
    // Backlog line 149: k6 parity — null/non-object conds throw, raw
    // check names (no "check " prefix), 3rd tags arg forwarded, and a
    // throwing predicate records a failed check then propagates.
    var check = function (val, conds, tags) {
        if (conds === null || conds === undefined || typeof conds !== 'object') {
            throw new TypeError('check() requires an object as its second argument');
        }
        var allPassed = true;
        var tagsJson = '';
        if (tags && typeof tags === 'object') {
            try { tagsJson = JSON.stringify(tags); } catch (e) { tagsJson = ''; }
        }
        var names = Object.keys(conds);
        for (var i = 0; i < names.length; i++) {
            var name = names[i];
            var condition = conds[name];
            var passed = false;
            if (typeof condition === 'function') {
                try {
                    passed = !!condition(val);
                } catch (e) {
                    if (typeof __tropel_pm_test === 'function') {
                        __tropel_pm_test(name, false, tagsJson);
                    }
                    throw e;
                }
            } else {
                passed = !!condition;
            }
            if (typeof __tropel_pm_test === 'function') {
                __tropel_pm_test(name, passed, tagsJson);
            }
            if (!passed) {
                allPassed = false;
            }
        }
        return allPassed;
    };
}

// group(name, fn) — defined in pm-api/pm.js if loaded, else here
if (typeof group !== 'function') {
    var group = function (name, fn) {
        if (typeof __tropel_pm_group_start === 'function') {
            __tropel_pm_group_start(name);
            var startTime = Date.now();
            try {
                if (typeof fn === 'function') {
                    return fn();
                }
            } finally {
                var duration = Date.now() - startTime;
                __tropel_pm_group_end(name, duration);
            }
        } else {
            if (typeof fn === 'function') {
                return fn();
            }
        }
    };
}

// sleep(seconds) — bootstrapped by the engine, but ensure it exists
if (typeof sleep !== 'function') {
    var sleep = function (seconds) {
        if (typeof __tropel_native_sleep === 'function') {
            __tropel_native_sleep(seconds * 1000);
        }
    };
}

// ══════════════════════════════════════════════════════════════════
// k6 globals
// ══════════════════════════════════════════════════════════════════

// __VU and __ITER — updated by K6DriverInstance before each iteration
var __VU = __VU || 0;
var __ITER = __ITER || 0;

// ══════════════════════════════════════════════════════════════════
// k6/metrics — Metric constructors
// ══════════════════════════════════════════════════════════════════

// These are also defined in pm-api/pm.js — only define if missing.
// NOTE: `var` assignments, not `function` declarations — avoids re-declaring
// when the guard condition is true.
if (typeof Counter !== 'function') {
    var Counter = function (name) {
        if (!name || typeof name !== 'string') {
            throw new Error('Counter requires a metric name');
        }
        this._name = name;
        this._type = 'counter';
        this._isTime = false;
    };
    Counter.prototype.add = function (value, tags) {
        if (typeof __tropel_pm_custom_metric_add === 'function') {
            var tagsStr = tags ? JSON.stringify(tags) : '{}';
            __tropel_pm_custom_metric_add(this._name, Number(value), tagsStr, this._type, this._isTime);
        }
        return this;
    };
}
if (typeof Gauge !== 'function') {
    var Gauge = function (name) {
        if (!name || typeof name !== 'string') {
            throw new Error('Gauge requires a metric name');
        }
        this._name = name;
        this._type = 'gauge';
        this._isTime = false;
    };
    Gauge.prototype.add = function (value, tags) {
        if (typeof __tropel_pm_custom_metric_add === 'function') {
            var tagsStr = tags ? JSON.stringify(tags) : '{}';
            __tropel_pm_custom_metric_add(this._name, Number(value), tagsStr, this._type, this._isTime);
        }
        return this;
    };
}
if (typeof Rate !== 'function') {
    var Rate = function (name) {
        if (!name || typeof name !== 'string') {
            throw new Error('Rate requires a metric name');
        }
        this._name = name;
        this._type = 'rate';
        this._isTime = false;
    };
    Rate.prototype.add = function (value, tags) {
        if (typeof __tropel_pm_custom_metric_add === 'function') {
            var tagsStr = tags ? JSON.stringify(tags) : '{}';
            __tropel_pm_custom_metric_add(this._name, Number(value), tagsStr, this._type, this._isTime);
        }
        return this;
    };
}
if (typeof Trend !== 'function') {
    // Backlog line 154: k6's Trend takes (name, isTime). isTime marks the
    // metric as containing time, so json-stream stamps `contains: "time"`
    // and summaries render it in ms even for nonstandard names (my_timer).
    var Trend = function (name, isTime) {
        if (!name || typeof name !== 'string') {
            throw new Error('Trend requires a metric name');
        }
        this._name = name;
        this._type = 'trend';
        this._isTime = isTime === true;
    };
    Trend.prototype.add = function (value, tags) {
        if (typeof __tropel_pm_custom_metric_add === 'function') {
            var tagsStr = tags ? JSON.stringify(tags) : '{}';
            __tropel_pm_custom_metric_add(this._name, Number(value), tagsStr, this._type, this._isTime);
        }
        return this;
    };
}

// ══════════════════════════════════════════════════════════════════
// k6/crypto + k6/encoding + k6/timers + randomSeed + k6/crypto/x509
// (backlog lines 125/126/131/132) — the missing module surfaces.
// ══════════════════════════════════════════════════════════════════

// ── byte marshalling helpers ──

function k6Utf8Encode(str) {
    var bytes = [];
    for (var i = 0; i < str.length; i++) {
        var c = str.charCodeAt(i);
        if (c < 0x80) {
            bytes.push(c);
        } else if (c < 0x800) {
            bytes.push(0xC0 | (c >> 6), 0x80 | (c & 0x3F));
        } else if (c >= 0xD800 && c <= 0xDBFF && i + 1 < str.length) {
            var lo = str.charCodeAt(i + 1);
            if (lo >= 0xDC00 && lo <= 0xDFFF) {
                var cp = 0x10000 + ((c - 0xD800) << 10) + (lo - 0xDC00);
                bytes.push(0xF0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3F), 0x80 | ((cp >> 6) & 0x3F), 0x80 | (cp & 0x3F));
                i++;
                continue;
            }
            bytes.push(0xEF, 0xBF, 0xBD);
        } else {
            bytes.push(0xE0 | (c >> 12), 0x80 | ((c >> 6) & 0x3F), 0x80 | (c & 0x3F));
        }
    }
    return bytes;
}

function k6Utf8Decode(bytes) {
    var out = '';
    for (var i = 0; i < bytes.length; i++) {
        var b = bytes[i];
        if (b < 0x80) {
            out += String.fromCharCode(b);
        } else if (b < 0xE0 && i + 1 < bytes.length) {
            out += String.fromCharCode(((b & 0x1F) << 6) | (bytes[i + 1] & 0x3F));
            i++;
        } else if (b < 0xF0 && i + 2 < bytes.length) {
            var cp = ((b & 0x0F) << 12) | ((bytes[i + 1] & 0x3F) << 6) | (bytes[i + 2] & 0x3F);
            out += String.fromCharCode(cp);
            i += 2;
        } else if (i + 3 < bytes.length) {
            var cp2 = ((b & 0x07) << 18) | ((bytes[i + 1] & 0x3F) << 12) | ((bytes[i + 2] & 0x3F) << 6) | (bytes[i + 3] & 0x3F);
            out += String.fromCharCode(0xD800 + ((cp2 - 0x10000) >> 10), 0xDC00 + ((cp2 - 0x10000) & 0x3FF));
            i += 3;
        } else {
            out += String.fromCharCode(b);
        }
    }
    return out;
}

// k6's common.ToBytes: string → UTF-8 bytes, ArrayBuffer → bytes.
function k6ToBytes(input) {
    if (typeof input === 'string') {
        return k6Utf8Encode(input);
    }
    if (input instanceof ArrayBuffer) {
        return Array.prototype.slice.call(new Uint8Array(input));
    }
    if (typeof Uint8Array !== 'undefined' && input instanceof Uint8Array) {
        return Array.prototype.slice.call(input);
    }
    throw new TypeError('k6 input must be a string or ArrayBuffer');
}

function k6BytesToHex(bytes) {
    var hex = '';
    for (var i = 0; i < bytes.length; i++) {
        var b = bytes[i] & 0xFF;
        hex += (b < 16 ? '0' : '') + b.toString(16);
    }
    return hex;
}

// ── k6/crypto (backlog line 126) ──

var __k6_native_hash = {};
if (typeof __tropel_native_md4 === 'function') __k6_native_hash.md4 = __tropel_native_md4;
if (typeof __tropel_native_md5 === 'function') __k6_native_hash.md5 = __tropel_native_md5;
if (typeof __tropel_native_sha1 === 'function') __k6_native_hash.sha1 = __tropel_native_sha1;
if (typeof __tropel_native_sha256 === 'function') __k6_native_hash.sha256 = __tropel_native_sha256;
if (typeof __tropel_native_sha384 === 'function') __k6_native_hash.sha384 = __tropel_native_sha384;
if (typeof __tropel_native_sha512 === 'function') __k6_native_hash.sha512 = __tropel_native_sha512;
if (typeof __tropel_native_sha512_224 === 'function') __k6_native_hash.sha512_224 = __tropel_native_sha512_224;
if (typeof __tropel_native_sha512_256 === 'function') __k6_native_hash.sha512_256 = __tropel_native_sha512_256;
if (typeof __tropel_native_ripemd160 === 'function') __k6_native_hash.ripemd160 = __tropel_native_ripemd160;

// k6's crypto Digest() output encodings: hex, base64, base64url (padded
// URLEncoding), base64rawurl (unpadded URLEncoding), binary (ArrayBuffer).
function k6DigestOutput(bytes, outputEncoding) {
    var enc = outputEncoding || 'hex';
    switch (enc) {
        case 'hex':
            return k6BytesToHex(bytes);
        case 'base64':
            return bytesToBase64(bytes);
        case 'base64url': {
            var s = bytesToBase64(bytes).replace(/\+/g, '-').replace(/\//g, '_');
            var r = s.length % 4;
            if (r === 2) return s + '==';
            if (r === 3) return s + '=';
            return s;
        }
        case 'base64rawurl':
            return bytesToBase64(bytes).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
        case 'binary':
            return new Uint8Array(bytes).buffer;
        default:
            throw new Error('invalid output encoding: ' + enc);
    }
}

// One-shot k6/crypto hash: (input, outputEncoding) → string | ArrayBuffer.
function k6OneShotHash(alg) {
    return function (input, outputEncoding) {
        var fn = __k6_native_hash[alg];
        if (typeof fn !== 'function') {
            throw new Error('k6/crypto: algorithm unavailable: ' + alg);
        }
        return k6DigestOutput(fn(k6ToBytes(input)), outputEncoding);
    };
}

// Stateful Hasher (createHash / createHMAC). Buffers inputs; the one-shot
// native hasher runs at digest() time — identical to k6's streaming hash
// for concatenated updates.
function K6Hasher(alg, keyBytes) {
    this.alg = alg;
    this.key = keyBytes || null;
    this.buf = [];
}
K6Hasher.prototype.update = function (input) {
    var b = k6ToBytes(input);
    for (var i = 0; i < b.length; i++) this.buf.push(b[i]);
    return this;
};
K6Hasher.prototype.digest = function (outputEncoding) {
    var out;
    if (this.key) {
        if (typeof __tropel_native_hmac !== 'function') {
            throw new Error('k6/crypto: hmac unavailable');
        }
        out = __tropel_native_hmac(this.alg, this.key, this.buf);
        if (!out) throw new Error('invalid algorithm: ' + this.alg);
    } else {
        var fn = __k6_native_hash[this.alg];
        if (typeof fn !== 'function') throw new Error('invalid algorithm: ' + this.alg);
        out = fn(this.buf);
    }
    return k6DigestOutput(out, outputEncoding);
};

function k6CreateHash(algorithm) {
    return new K6Hasher(algorithm, null);
}

function k6CreateHmac(algorithm, secret) {
    return new K6Hasher(algorithm, k6ToBytes(secret));
}

function k6Hmac(algorithm, secret, input, outputEncoding) {
    if (typeof __tropel_native_hmac !== 'function') {
        throw new Error('k6/crypto: hmac unavailable');
    }
    var out = __tropel_native_hmac(algorithm, k6ToBytes(secret), k6ToBytes(input));
    if (!out) throw new Error('invalid algorithm: ' + algorithm);
    return k6DigestOutput(out, outputEncoding);
}

function k6RandomBytes(size) {
    if (typeof size !== 'number' || size < 1) {
        throw new Error('invalid size');
    }
    var bytes = __tropel_native_random_bytes(size);
    return new Uint8Array(bytes).buffer;
}

function k6HexEncode(input) {
    return k6BytesToHex(k6ToBytes(input));
}

var crypto = {};
crypto.md4 = k6OneShotHash('md4');
crypto.md5 = k6OneShotHash('md5');
crypto.sha1 = k6OneShotHash('sha1');
crypto.sha256 = k6OneShotHash('sha256');
crypto.sha384 = k6OneShotHash('sha384');
crypto.sha512 = k6OneShotHash('sha512');
crypto.sha512_224 = k6OneShotHash('sha512_224');
crypto.sha512_256 = k6OneShotHash('sha512_256');
crypto.ripemd160 = k6OneShotHash('ripemd160');
crypto.hmac = k6Hmac;
crypto.createHash = k6CreateHash;
crypto.createHMAC = k6CreateHmac;
crypto.randomBytes = k6RandomBytes;
crypto.hexEncode = k6HexEncode;

// Named-import parity: `import { sha256 } from 'k6/crypto'` is stripped to a
// bare `sha256(...)` call, so every export is also a bare global (mirrors how
// check/group/sleep/fail are exposed). Guarded so we never clobber an
// existing binding.
// NOTE: the `crypto` object itself is intentionally NOT exposed as a bare
// global — only its members are (k6 exports the functions, not a namespace).
// Keeping `crypto` unclaimed also avoids colliding with a future WebCrypto
// global or a user script that declares `var crypto`.
// NOTE: each fallback declares the binding with `var` inside the block. A
// bare `name = value` assignment to a never-declared global is a ReferenceError
// under QuickJS's strict eval; the hoisted `var` makes it a legal assignment.
if (typeof md4 === 'undefined') { var md4 = crypto.md4; }
if (typeof md5 === 'undefined') { var md5 = crypto.md5; }
if (typeof sha1 === 'undefined') { var sha1 = crypto.sha1; }
if (typeof sha256 === 'undefined') { var sha256 = crypto.sha256; }
if (typeof sha384 === 'undefined') { var sha384 = crypto.sha384; }
if (typeof sha512 === 'undefined') { var sha512 = crypto.sha512; }
if (typeof sha512_224 === 'undefined') { var sha512_224 = crypto.sha512_224; }
if (typeof sha512_256 === 'undefined') { var sha512_256 = crypto.sha512_256; }
if (typeof ripemd160 === 'undefined') { var ripemd160 = crypto.ripemd160; }
if (typeof hmac === 'undefined') { var hmac = crypto.hmac; }
if (typeof createHash === 'undefined') { var createHash = crypto.createHash; }
if (typeof createHMAC === 'undefined') { var createHMAC = crypto.createHMAC; }
if (typeof randomBytes === 'undefined') { var randomBytes = crypto.randomBytes; }
if (typeof hexEncode === 'undefined') { var hexEncode = crypto.hexEncode; }

// ── k6/encoding (backlog line 125) ──
// b64encode(input, encoding) / b64decode(input, encoding, format).
// encodings: std (default), rawstd, url, rawurl. Unknown → silently std.
// b64decode format: 's' → string, anything else → ArrayBuffer.

function k6B64Encode(input, encodingName) {
    var enc = encodingName || 'std';
    var bytes = k6ToBytes(input);
    var isUrl = (enc === 'url' || enc === 'rawurl');
    var raw = (enc === 'rawstd' || enc === 'rawurl');
    var s;
    if (isUrl) {
        // native URL_SAFE_NO_PAD; k6's 'url' adds padding, 'rawurl' doesn't.
        s = __tropel_native_base64url_encode(bytes);
        if (!raw) {
            var r = s.length % 4;
            if (r === 2) s += '==';
            else if (r === 3) s += '=';
        }
    } else {
        s = __tropel_native_base64_encode(bytes); // STANDARD, padded
        if (raw) s = s.replace(/=+$/, '');
    }
    return s;
}

function k6B64Decode(input, encodingName, format) {
    var enc = encodingName || 'std';
    var s = typeof input === 'string' ? input : k6Utf8Decode(k6ToBytes(input));
    var isUrl = (enc === 'url' || enc === 'rawurl');
    var norm = s;
    if (isUrl) norm = s.replace(/-/g, '+').replace(/_/g, '/');
    var r = norm.length % 4;
    if (r === 2) norm += '==';
    else if (r === 3) norm += '=';
    var bytes = __tropel_native_base64_decode(norm);
    if (!bytes) throw new Error('invalid base64 data');
    if (format === 's') {
        // Known approximation: k6 (Go) returns the raw bytes as a Go string,
        // which goja surfaces with U+FFFD replacement for invalid UTF-8; our
        // k6Utf8Decode keeps the bytes as Latin-1-ish code units instead.
        return k6Utf8Decode(bytes);
    }
    return new Uint8Array(bytes).buffer;
}

var encoding = {};
encoding.b64encode = k6B64Encode;
encoding.b64decode = k6B64Decode;
if (typeof b64encode === 'undefined') { var b64encode = k6B64Encode; }
if (typeof b64decode === 'undefined') { var b64decode = k6B64Decode; }

// ── k6/timers (backlog line 131) — globals, the module is a pure re-export ──
// k6's timers fire on the VU event loop; Tropel runs JS synchronously, so
// due timers are pumped by the driver at iteration boundaries via
// __tropel_pump_timers(). This is what unblocks lodash debounce/throttle.

var __tropel_timers = {};
var __tropel_timer_seq = 0;

// Backlog line 99: timers leaked across iterations. __tropel_timers is
// module-scope and had no per-iteration reset, so a setInterval armed in
// every iteration accumulated 3 live intervals all firing on every
// subsequent pump — linear growth in callbacks and retained closures for
// the VU's life. The driver calls __tropel_reset_timers() at the start of
// each iteration so only the CURRENT iteration's timers are live: ones
// armed in iteration N fire at the N boundary pump, then are cleared.
// NOTE: the id sequence is NOT reset — a user-held timer id from an earlier
// iteration must never collide with a fresh timer's id (stale clearInterval
// would kill the wrong timer). Only the live table is cleared.
function __tropel_reset_timers() {
    __tropel_timers = {};
}

function setTimeout(fn, ms) {
    var id = ++__tropel_timer_seq;
    __tropel_timers[id] = { fn: fn, at: Date.now() + (ms || 0), interval: false, ms: ms || 0 };
    return id;
}
function clearTimeout(id) { delete __tropel_timers[id]; }
function setInterval(fn, ms) {
    var id = ++__tropel_timer_seq;
    __tropel_timers[id] = { fn: fn, at: Date.now() + (ms || 0), interval: true, ms: ms || 0 };
    return id;
}
function clearInterval(id) { delete __tropel_timers[id]; }

// NOTE: ids are snapshotted, so a timer REGISTERED inside a callback only
// fires on the next pump — deliberate, avoids infinite same-pump re-triggers.
// A throwing callback is caught and dropped (k6 logs and continues): for
// intervals the re-arm already happened, so the interval keeps firing;
// one-shots are already deleted, so they don't retry.
function __tropel_pump_timers() {
    var now = Date.now();
    var ids = Object.keys(__tropel_timers);
    for (var i = 0; i < ids.length; i++) {
        var id = ids[i];
        var t = __tropel_timers[id];
        if (!t || now < t.at) continue;
        if (t.interval) {
            t.at = now + t.ms;
        } else {
            delete __tropel_timers[id];
        }
        try {
            t.fn();
        } catch (e) {
            // Ignore: k6 reports timer errors without aborting the iteration.
        }
    }
}

// ── randomSeed(seed) (backlog line 132) — per-VU deterministic Math.random ──
// Each VU owns its JsContext, so replacing Math.random here is naturally
// per-VU, exactly k6's contract. k6 requires an integer seed.
function randomSeed(seed) {
    if (typeof seed !== 'number' || !isFinite(seed) || Math.floor(seed) !== seed) {
        throw new TypeError('randomSeed requires an integer seed');
    }
    var state = seed >>> 0;
    Math.random = function () {
        // mulberry32
        state = (state + 0x6D2B79F5) | 0;
        var t = state;
        t = Math.imul(t ^ (t >>> 15), t | 1);
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
}

// ── k6/crypto/x509 (backlog line 126) — parse / getSubject / getIssuer /
// getAltNames. Minimal DER reader for the standard certificate layout.
// ──

var x509 = {};

var X509_SIG_OID_NAMES = {
    '1.2.840.113549.1.1.5': 'SHA1-RSA',
    '1.2.840.113549.1.1.11': 'SHA256-RSA',
    '1.2.840.113549.1.1.12': 'SHA384-RSA',
    '1.2.840.113549.1.1.13': 'SHA512-RSA',
    '1.2.840.113549.1.1.4': 'MD5-RSA',
    '1.2.840.10045.4.1': 'ECDSA-SHA1',
    '1.2.840.10045.4.3.2': 'ECDSA-SHA256',
    '1.2.840.10045.4.3.3': 'ECDSA-SHA384',
    '1.2.840.10045.4.3.4': 'ECDSA-SHA512'
};

// Read one DER TLV from bytes at pos. Returns {tag, valueStart, valueEnd, end}.
function x509ReadTlv(bytes, pos) {
    var tag = bytes[pos];
    var p = pos + 1;
    var len = bytes[p++];
    if (len & 0x80) {
        var n = len & 0x7F;
        len = 0;
        for (var i = 0; i < n; i++) len = (len << 8) | bytes[p++];
    }
    return { tag: tag, valueStart: p, valueEnd: p + len, end: p + len };
}

function x509ReadOid(bytes) {
    var vals = [];
    var b0 = bytes[0];
    vals.push(Math.floor(b0 / 40), b0 % 40);
    var v = 0;
    for (var i = 1; i < bytes.length; i++) {
        v = (v << 7) | (bytes[i] & 0x7F);
        if ((bytes[i] & 0x80) === 0) { vals.push(v); v = 0; }
    }
    return vals.join('.');
}

function x509StringFromTlv(bytes, tag) {
    if (tag === 0x1E) { // BMPString: 2 bytes per code unit
        var s = '';
        for (var i = 0; i + 1 < bytes.length; i += 2) {
            s += String.fromCharCode((bytes[i] << 8) | bytes[i + 1]);
        }
        return s;
    }
    return k6Utf8Decode(bytes);
}

// Name ::= RDNSequence ::= SEQUENCE OF RDN; RDN ::= SET OF ATV;
// ATV ::= SEQUENCE { OID, value }. Returns [{type, value}].
function x509ParseName(bytes) {
    var attrs = [];
    var p = 0;
    while (p < bytes.length) {
        var rdn = x509ReadTlv(bytes, p); // SET
        var q = rdn.valueStart;
        while (q < rdn.valueEnd) {
            var atv = x509ReadTlv(bytes, q); // SEQUENCE
            var oid = x509ReadTlv(bytes, atv.valueStart);
            var val = x509ReadTlv(bytes, oid.end);
            attrs.push({
                type: x509ReadOid(bytes.slice(oid.valueStart, oid.valueEnd)),
                value: x509StringFromTlv(bytes.slice(val.valueStart, val.valueEnd), val.tag)
            });
            q = atv.end;
        }
        p = rdn.end;
    }
    return attrs;
}

function x509ParseTime(bytes, tag) {
    var s = k6Utf8Decode(bytes);
    var year, rest;
    if (tag === 0x17) { // UTCTime: YYMMDDHHMMSSZ, 2-digit year
        var yy = parseInt(s.substr(0, 2), 10);
        year = (yy >= 50 ? 1900 : 2000) + yy;
        rest = s.substr(2);
    } else { // GeneralizedTime: YYYYMMDDHHMMSSZ
        year = parseInt(s.substr(0, 4), 10);
        rest = s.substr(4);
    }
    return year + '-' + rest.substr(0, 2) + '-' + rest.substr(2, 2) +
        'T' + rest.substr(4, 2) + ':' + rest.substr(6, 2) + ':' + rest.substr(8, 2) + 'Z';
}

function x509SubjectObject(attrs) {
    var obj = {
        commonName: '', country: '', postalCode: '', stateOrProvinceName: '',
        localityName: '', streetAddress: '', organizationName: '',
        organizationalUnitName: [], names: attrs
    };
    for (var i = 0; i < attrs.length; i++) {
        var t = attrs[i].type, v = attrs[i].value;
        if (t === '2.5.4.3' && !obj.commonName) obj.commonName = v;
        else if (t === '2.5.4.6' && !obj.country) obj.country = v;
        else if (t === '2.5.4.17' && !obj.postalCode) obj.postalCode = v;
        else if (t === '2.5.4.8' && !obj.stateOrProvinceName) obj.stateOrProvinceName = v;
        else if (t === '2.5.4.7' && !obj.localityName) obj.localityName = v;
        else if (t === '2.5.4.9' && !obj.streetAddress) obj.streetAddress = v;
        else if (t === '2.5.4.10' && !obj.organizationName) obj.organizationName = v;
        else if (t === '2.5.4.11') obj.organizationalUnitName.push(v);
    }
    return obj;
}

function x509IssuerObject(attrs) {
    var obj = {
        commonName: '', country: '', stateOrProvinceName: '',
        localityName: '', organizationName: '', names: attrs
    };
    for (var i = 0; i < attrs.length; i++) {
        var t = attrs[i].type, v = attrs[i].value;
        if (t === '2.5.4.3' && !obj.commonName) obj.commonName = v;
        else if (t === '2.5.4.6' && !obj.country) obj.country = v;
        else if (t === '2.5.4.8' && !obj.stateOrProvinceName) obj.stateOrProvinceName = v;
        else if (t === '2.5.4.7' && !obj.localityName) obj.localityName = v;
        else if (t === '2.5.4.10' && !obj.organizationName) obj.organizationName = v;
    }
    return obj;
}

// k6 altNames order: DNSNames, EmailAddresses, IPs, URIs.
function x509AltNames(exts) {
    var dns = [], email = [], ip = [], uri = [];
    for (var i = 0; i < exts.length; i++) {
        if (exts[i].oid !== '2.5.29.17') continue;
        var v = exts[i].value;
        var seq = x509ReadTlv(v, 0); // GeneralNames ::= SEQUENCE OF GeneralName
        var p = seq.valueStart;
        while (p < seq.valueEnd) {
            var gn = x509ReadTlv(v, p);
            var val = v.slice(gn.valueStart, gn.valueEnd);
            if (gn.tag === 0x82) dns.push(k6Utf8Decode(val));       // dNSName
            else if (gn.tag === 0x81) email.push(k6Utf8Decode(val)); // rfc822Name
            else if (gn.tag === 0x87) {                              // iPAddress
                var parts = [];
                for (var k = 0; k < val.length; k++) parts.push(val[k]);
                ip.push(parts.join('.'));
            } else if (gn.tag === 0x86) uri.push(k6Utf8Decode(val)); // URI
            p = gn.end;
        }
    }
    return dns.concat(email, ip, uri);
}

function x509ParseExtensions(extBytes) {
    var exts = [];
    var p = 0;
    while (p < extBytes.length) {
        var ext = x509ReadTlv(extBytes, p); // SEQUENCE
        var oid = x509ReadTlv(extBytes, ext.valueStart);
        var q = oid.end;
        var first = x509ReadTlv(extBytes, q);
        var octet;
        if (first.tag === 0x01) octet = x509ReadTlv(extBytes, first.end); // critical BOOLEAN
        else octet = first;
        exts.push({
            oid: x509ReadOid(extBytes.slice(oid.valueStart, oid.valueEnd)),
            value: extBytes.slice(octet.valueStart, octet.valueEnd)
        });
        p = ext.end;
    }
    return exts;
}

// Decode PEM armor → DER bytes.
function x509PemToDer(pem) {
    var b64 = String(pem).replace(/-----[^-]+-----/g, '').replace(/\s+/g, '');
    var bytes = __tropel_native_base64_decode(b64);
    if (!bytes) throw new Error('failed to decode certificate PEM file');
    return bytes;
}

function x509ParseCert(encoded) {
    var der = x509PemToDer(encoded);
    var tlv = x509ReadTlv(der, 0); // Certificate ::= SEQUENCE
    var p = tlv.valueStart;

    var tbs = x509ReadTlv(der, p); // TBSCertificate ::= SEQUENCE
    var tbsBytes = der.slice(tbs.valueStart, tbs.valueEnd);

    // Walk TBSCertificate fields: version [0]?, serial, sigAlg, issuer,
    // validity, subject, spki, then optional [1]/[2]/[3] extensions.
    var items = [];
    var q = 0;
    while (q < tbsBytes.length) {
        var it = x509ReadTlv(tbsBytes, q);
        items.push({ tag: it.tag, start: it.valueStart, end: it.valueEnd, bytes: tbsBytes.slice(it.valueStart, it.valueEnd) });
        q = it.end;
    }
    var idx = 0;
    if (items[idx] && items[idx].tag === 0xA0) idx++; // version [0] EXPLICIT
    // idx+0 serialNumber, idx+1 signature AlgId, idx+2 issuer, idx+3 validity,
    // idx+4 subject, idx+5 spki, idx+6.. extensions.
    var sigAlg = items[idx + 1];
    var issuer = items[idx + 2];
    var validity = items[idx + 3];
    var subject = items[idx + 4];
    var spki = items[idx + 5];

    var issuerAttrs = x509ParseName(issuer.bytes);
    var subjectAttrs = x509ParseName(subject.bytes);

    // validity ::= SEQUENCE { notBefore Time, notAfter Time }
    var nb = x509ReadTlv(validity.bytes, 0);
    var na = x509ReadTlv(validity.bytes, nb.end);

    // signatureAlgorithm: tbs signature AlgId OID → Go String() name.
    var sigOid = x509ReadTlv(sigAlg.bytes, 0);
    var sigOidStr = x509ReadOid(sigAlg.bytes.slice(sigOid.valueStart, sigOid.valueEnd));
    var signatureAlgorithm = X509_SIG_OID_NAMES[sigOidStr] || 'UnknownSignatureAlgorithm';

    // publicKey.algorithm from SPKI AlgId OID.
    var spkiAlg = x509ReadTlv(spki.bytes, 0);
    var spkiOid = x509ReadTlv(spki.bytes, spkiAlg.valueStart);
    var spkiOidStr = x509ReadOid(spki.bytes.slice(spkiOid.valueStart, spkiOid.valueEnd));
    var keyAlgorithm = 'Unknown';
    if (spkiOidStr === '1.2.840.113549.1.1.1') keyAlgorithm = 'RSA';
    else if (spkiOidStr === '1.2.840.10045.2.1') keyAlgorithm = 'ECDSA';
    else if (spkiOidStr === '1.2.840.10040.4.1') keyAlgorithm = 'DSA';

    // extensions [3] EXPLICIT (0xA3) → Extensions ::= SEQUENCE OF Extension.
    // The 0xA3 value is the WRAPPING Extensions SEQUENCE; descend one level so
    // each top-level TLV is a single Extension (else only the first extension
    // — usually subjectKeyIdentifier, never the SAN — is parsed).
    var extItems = [];
    for (var e = idx + 6; e < items.length; e++) {
        if (items[e].tag === 0xA3) {
            var wrap = x509ReadTlv(items[e].bytes, 0);
            extItems = items[e].bytes.slice(wrap.valueStart, wrap.valueEnd);
        }
    }
    var exts = extItems.length ? x509ParseExtensions(extItems) : [];
    var altNames = x509AltNames(exts);

    // fingerPrint: SHA-1 of the raw cert DER (k6: sha1.Sum(parsed.Raw)).
    var fingerPrint = __tropel_native_sha1(der);

    return {
        subject: x509SubjectObject(subjectAttrs),
        issuer: x509IssuerObject(issuerAttrs),
        notBefore: x509ParseTime(validity.bytes.slice(nb.valueStart, nb.valueEnd), nb.tag),
        notAfter: x509ParseTime(validity.bytes.slice(na.valueStart, na.valueEnd), na.tag),
        altNames: altNames,
        signatureAlgorithm: signatureAlgorithm,
        fingerPrint: fingerPrint,
        publicKey: { algorithm: keyAlgorithm, key: spki.bytes }
    };
}

x509.parse = function (encoded) {
    return x509ParseCert(encoded);
};
x509.getSubject = function (encoded) {
    return x509ParseCert(encoded).subject;
};
x509.getIssuer = function (encoded) {
    return x509ParseCert(encoded).issuer;
};
x509.getAltNames = function (encoded) {
    return x509ParseCert(encoded).altNames;
};
if (typeof parse === 'undefined') { var parse = x509.parse; }
if (typeof getSubject === 'undefined') { var getSubject = x509.getSubject; }
if (typeof getIssuer === 'undefined') { var getIssuer = x509.getIssuer; }
if (typeof getAltNames === 'undefined') { var getAltNames = x509.getAltNames; }

// ══════════════════════════════════════════════════════════════════
// Helpers
// ══════════════════════════════════════════════════════════════════

function getStatusText(code) {
    var texts = {
        200: 'OK', 201: 'Created', 204: 'No Content',
        301: 'Moved Permanently', 302: 'Found', 304: 'Not Modified',
        400: 'Bad Request', 401: 'Unauthorized', 403: 'Forbidden',
        404: 'Not Found', 405: 'Method Not Allowed', 408: 'Request Timeout',
        409: 'Conflict', 413: 'Payload Too Large', 415: 'Unsupported Media Type',
        422: 'Unprocessable Entity', 429: 'Too Many Requests',
        500: 'Internal Server Error', 502: 'Bad Gateway',
        503: 'Service Unavailable', 504: 'Gateway Timeout'
    };
    return texts[code] || '';
}
