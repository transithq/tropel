/**
 * `fetch` for the tropel script realm (TR-475).
 *
 * QuickJS ships no `fetch`, so a script that used one — the shape every
 * modern API-client script is written in — got a bare ReferenceError. The
 * realm already has an HTTP client behind `__tropel_trp_send_request`
 * (TR-472); this is that binding wearing the interface people expect.
 *
 * NOT a new transport. It is the SAME client `pm.sendRequest` uses, so a
 * script's fetch inherits the same posture, and there is one network path
 * out of the realm rather than two that can disagree.
 *
 * The binding is checked at CALL time, never at eval time: the shim bundle
 * is evaluated BEFORE the bridge is installed, so an eval-time
 * `typeof __tropel_trp_send_request` is always "undefined" and would leave
 * `fetch` permanently unset. pm.js defers the same check for the same
 * reason.
 */
(function () {
    var g = typeof globalThis !== 'undefined' ? globalThis : this;

    // Never shadow a real one. A host that already provides `fetch` (a
    // browser, or a future runtime that grows one) keeps its own — this shim
    // exists only where there is none.
    if (typeof g.fetch === 'function') {
        return;
    }

    function headerPairs(headers) {
        var out = {};
        if (!headers) return out;
        if (typeof headers.forEach === 'function' && typeof headers.get === 'function') {
            headers.forEach(function (v, k) { out[k] = String(v); });
            return out;
        }
        if (Object.prototype.toString.call(headers) === '[object Array]') {
            for (var i = 0; i < headers.length; i++) {
                var pair = headers[i];
                if (pair && pair.length >= 2) out[String(pair[0])] = String(pair[1]);
            }
            return out;
        }
        for (var k in headers) {
            if (Object.prototype.hasOwnProperty.call(headers, k)) out[k] = String(headers[k]);
        }
        return out;
    }

    /** A minimal Response. `text()`/`json()` return promises, as the real one
     *  does — a script that awaits them must not get a raw value back. */
    function responseOf(raw) {
        var parsed;
        try {
            parsed = JSON.parse(raw);
        } catch (e) {
            throw new Error('fetch: the host answered with invalid JSON: ' + raw);
        }
        // The bridge reports transport failures in-band; surface them as a
        // rejected fetch rather than a 0-status response a script would read
        // as "the server answered" (invariant 8).
        if (parsed && parsed.error && !parsed.code) {
            throw new Error('fetch: ' + parsed.error);
        }
        var status = parsed.code || 0;
        var headers = parsed.headers || {};
        var lower = {};
        for (var k in headers) {
            if (Object.prototype.hasOwnProperty.call(headers, k)) {
                lower[String(k).toLowerCase()] = String(headers[k]);
            }
        }
        return {
            status: status,
            ok: status >= 200 && status < 300,
            statusText: parsed.statusText || '',
            url: parsed.url || '',
            headers: {
                get: function (name) {
                    var v = lower[String(name).toLowerCase()];
                    return v === undefined ? null : v;
                },
                has: function (name) {
                    return Object.prototype.hasOwnProperty.call(lower, String(name).toLowerCase());
                }
            },
            text: function () { return Promise.resolve(parsed.body || ''); },
            json: function () {
                return new Promise(function (resolve, reject) {
                    try {
                        resolve(JSON.parse(parsed.body || ''));
                    } catch (e) {
                        reject(new Error('fetch: response body is not JSON'));
                    }
                });
            }
        };
    }

    g.fetch = function (input, init) {
        return new Promise(function (resolve, reject) {
            try {
                if (typeof __tropel_trp_send_request !== 'function') {
                    throw new Error(
                        'fetch is not available here: this realm was built without an HTTP ' +
                        'client, so there is nothing to send on (tropel TR-472).'
                    );
                }
                var opts = init || {};
                var url = typeof input === 'string' ? input : (input && input.url) || '';
                if (!url) throw new Error('fetch: a URL is required');
                var method = String(opts.method || (input && input.method) || 'GET').toUpperCase();
                var headers = headerPairs(opts.headers || (input && input.headers));
                var body = opts.body === undefined || opts.body === null ? '' : String(opts.body);
                var timeout = typeof opts.timeout === 'number' ? opts.timeout : 0;

                var raw = __tropel_trp_send_request(
                    method, String(url), JSON.stringify(headers), body, timeout, 'text'
                );
                resolve(responseOf(raw));
            } catch (e) {
                reject(e);
            }
        });
    };
})();
