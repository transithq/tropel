// ─── pm.* API for Tropel ─────────────────────────────────
// This JS glue layer provides the Postman pm.* API surface.
// It delegates heavy operations to native Rust functions.

// P4b: the binding is built by a factory so the namespace is a parameter —
// `pm` (frozen Postman-compat) and `trp` (canonical, Postman convention:
// `pm.*` is Postman's sole namespace, so `trp.*` is Tropel's) are peer
// views over the same shared state. Product aliases are opt-in via
// SandboxConfig, not installed by default.
function __tropel_build_binding(namespace) {
    var pm = {};
    var __ns = namespace || 'pm';

// TR-243: detect async functions for check()/group() rejection (k6 parity).
// An async function's `Symbol.toStringTag` is 'AsyncFunction'.
function isAsyncFunction(fn) {
    return Object.prototype.toString.call(fn) === '[object AsyncFunction]';
}

// ── pm.environment ──
// Backlog line 89: set/get must be INVERSES. The old setter String()-coerced
// (pm.response.json() → "[object Object]" stored) and the getter returned raw
// — an object set once could never come back. Values are now JSON-encoded on
// set (strings stay strings: '1234' is stored as '"1234"', never retyped to
// the number 1234) and JSON.parsed on get with a raw fallback. The runner's
// build_scope decodes the same encoding for {{var}} substitution.
pm.environment = {
    get: function (key) {
        if (typeof __tropel_trp_environment_get === 'function') {
            var raw = __tropel_trp_environment_get(key);
            if (raw === null || raw === undefined) return null;
            try { return JSON.parse(raw); } catch (e) { return raw; }
        }
        return null;
    },
    set: function (key, value) {
        if (typeof __tropel_trp_environment_set === 'function') {
            var encoded;
            try { encoded = value === undefined ? '' : JSON.stringify(value); }
            catch (e) { encoded = value === undefined ? '' : String(value); }
            __tropel_trp_environment_set(key, encoded);
        }
    },
    unset: function (key) {
        if (typeof __tropel_trp_environment_unset === 'function') {
            __tropel_trp_environment_unset(key);
        }
    },
    clear: function () {
        if (typeof __tropel_trp_environment_clear === 'function') {
            __tropel_trp_environment_clear();
        }
    },
    // Backlog line 145: Postman's pm.environment exposes has()/toObject()/
    // replaceIn() alongside get/set/unset/clear.
    has: function (key) {
        if (typeof __tropel_trp_environment_has === 'function') {
            return __tropel_trp_environment_has(key);
        }
        return pm.environment.get(key) !== null;
    },
    toObject: function () {
        if (typeof __tropel_trp_environment_to_object === 'function') {
            var map = __tropel_trp_environment_to_object() || {};
            var out = {};
            for (var k in map) {
                if (map.hasOwnProperty(k)) {
                    try { out[k] = JSON.parse(map[k]); } catch (e) { out[k] = map[k]; }
                }
            }
            return out;
        }
        return {};
    },
    replaceIn: function (text) {
        return pm.variables.replaceIn(text);
    }
};

// ── pm.collectionVariables ──
// Backlog line 145: one of the top-3 most-used pm.* members was entirely
// missing. Collection-scoped store; values round-trip JSON-encoded through
// the bridge like pm.variables (JSON.parse restores the correct type).
pm.collectionVariables = {
    get: function (key) {
        if (typeof __tropel_trp_collection_vars_get === 'function') {
            var raw = __tropel_trp_collection_vars_get(key);
            if (raw === null || raw === undefined) return undefined;
            try { return JSON.parse(raw); } catch (e) { return raw; }
        }
        return undefined;
    },
    set: function (key, value) {
        if (typeof __tropel_trp_collection_vars_set === 'function') {
            var encoded;
            try { encoded = value === undefined ? '' : JSON.stringify(value); }
            catch (e) { encoded = value === undefined ? '' : String(value); }
            __tropel_trp_collection_vars_set(key, encoded);
        }
    },
    unset: function (key) {
        if (typeof __tropel_trp_collection_vars_unset === 'function') {
            __tropel_trp_collection_vars_unset(key);
        }
    },
    has: function (key) {
        if (typeof __tropel_trp_collection_vars_has === 'function') {
            return __tropel_trp_collection_vars_has(key);
        }
        return false;
    },
    toObject: function () {
        if (typeof __tropel_trp_collection_vars_to_object === 'function') {
            var map = __tropel_trp_collection_vars_to_object() || {};
            var out = {};
            for (var k in map) {
                if (map.hasOwnProperty(k)) {
                    try { out[k] = JSON.parse(map[k]); } catch (e) { out[k] = map[k]; }
                }
            }
            return out;
        }
        return {};
    },
    replaceIn: function (text) {
        return pm.variables.replaceIn(text);
    }
};

// ── pm.globals ──
// Backlog line 145: global-scoped variable store, lowest-precedence scope.
pm.globals = {
    get: function (key) {
        if (typeof __tropel_trp_globals_get === 'function') {
            var raw = __tropel_trp_globals_get(key);
            if (raw === null || raw === undefined) return undefined;
            try { return JSON.parse(raw); } catch (e) { return raw; }
        }
        return undefined;
    },
    set: function (key, value) {
        if (typeof __tropel_trp_globals_set === 'function') {
            var encoded;
            try { encoded = value === undefined ? '' : JSON.stringify(value); }
            catch (e) { encoded = value === undefined ? '' : String(value); }
            __tropel_trp_globals_set(key, encoded);
        }
    },
    unset: function (key) {
        if (typeof __tropel_trp_globals_unset === 'function') {
            __tropel_trp_globals_unset(key);
        }
    },
    has: function (key) {
        if (typeof __tropel_trp_globals_has === 'function') {
            return __tropel_trp_globals_has(key);
        }
        return false;
    },
    toObject: function () {
        if (typeof __tropel_trp_globals_to_object === 'function') {
            var map = __tropel_trp_globals_to_object() || {};
            var out = {};
            for (var k in map) {
                if (map.hasOwnProperty(k)) {
                    try { out[k] = JSON.parse(map[k]); } catch (e) { out[k] = map[k]; }
                }
            }
            return out;
        }
        return {};
    },
    replaceIn: function (text) {
        return pm.variables.replaceIn(text);
    }
};

// ── pm.variables ──
pm.variables = {
    get: function (key) {
        if (typeof __tropel_trp_variables_get === 'function') {
            var raw = __tropel_trp_variables_get(key);
            if (raw === null || raw === undefined) return null;
            // Try JSON.parse — non-string values (objects, arrays, numbers,
            // booleans) come JSON-encoded from the bridge. If parse fails,
            // it's a plain string (return as-is).
            try { return JSON.parse(raw); }
            catch (e) { return raw; }
        }
        return null;
    },
    set: function (key, value) {
        if (typeof __tropel_trp_variables_set === 'function') {
            // Backlog line 89/146: JSON-encode so set/get are inverses (an
            // object set once comes back as an object; '1234' stays the
            // string '1234'). Line 146: never pass the RAW value into the
            // strict String bridge param.
            var encoded;
            try { encoded = value === undefined ? '' : JSON.stringify(value); }
            catch (e) { encoded = value === undefined ? '' : String(value); }
            __tropel_trp_variables_set(key, encoded);
        }
    },
    unset: function (key) {
        if (typeof __tropel_trp_variables_unset === 'function') {
            __tropel_trp_variables_unset(key);
        }
    },
    replaceIn: function (text) {
        // Simple variable replacement
        if (!text) return text;
        return text.replace(/\{\{([^}]+)\}\}/g, function (match, key) {
            var val = pm.variables.get(key.trim());
            return val !== null && val !== undefined ? String(val) : match;
        });
    }
};

// ── pm.response ──
// Backlog line 143: Postman exposes code/status/responseTime/headers/cookies
// as VALUE PROPERTIES, not functions. The old function-object form broke the
// two canonical idioms: `pm.expect(pm.response.code).to.eql(200)` compared a
// Function to 200 (never eql), and `pm.response.headers.get('X')` threw a
// TypeError (headers was a function). Only text()/json() are methods in
// Postman; the rest are values.
//
// The native __tropel_trp_response_* bridges are registered LAZILY (first
// iteration, after this shim bootstraps), so these are GETTERS that re-fetch
// the bridge on every read — same pattern as exec.js. That keeps the read
// shape a plain number/string/object exactly like Postman while staying live.
pm.response = {};

Object.defineProperty(pm.response, 'code', {
    get: function () {
        if (typeof globalThis !== 'undefined' && typeof globalThis.__tropel_trp_response_code === 'function') {
            return globalThis.__tropel_trp_response_code();
        }
        return 0;
    },
    enumerable: true,
    configurable: true
});

Object.defineProperty(pm.response, 'status', {
    get: function () {
        if (typeof globalThis !== 'undefined' && typeof globalThis.__tropel_trp_response_status === 'function') {
            return globalThis.__tropel_trp_response_status();
        }
        return '';
    },
    enumerable: true,
    configurable: true
});

Object.defineProperty(pm.response, 'responseTime', {
    get: function () {
        if (typeof globalThis !== 'undefined' && typeof globalThis.__tropel_trp_response_time === 'function') {
            return globalThis.__tropel_trp_response_time();
        }
        return 0;
    },
    enumerable: true,
    configurable: true
});

// Postman's pm.response.headers is a Headers object: pm.response.headers.get('X')
// is the canonical idiom. Returns a fresh object per read (the underlying map
// can change between iterations; the get() delegate is case-insensitive via the
// __tropel_trp_response_header bridge, matching Postman).
Object.defineProperty(pm.response, 'headers', {
    get: function () {
        var map = {};
        if (typeof globalThis !== 'undefined' && typeof globalThis.__tropel_trp_response_headers === 'function') {
            map = globalThis.__tropel_trp_response_headers() || {};
        }
        return {
            get: function (key) {
                if (typeof globalThis !== 'undefined' && typeof globalThis.__tropel_trp_response_header === 'function') {
                    var v = globalThis.__tropel_trp_response_header(key);
                    return v !== undefined && v !== null ? v : undefined;
                }
                return undefined;
            },
            all: function () { return map; },
            toObject: function () { return map; },
            count: function () {
                var n = 0;
                for (var k in map) { if (map.hasOwnProperty(k)) n++; }
                return n;
            }
        };
    },
    enumerable: true,
    configurable: true
});

// Postman's pm.response.cookies is a list of Cookie objects; scripts also use
// pm.response.cookies.get('name'). Returns an array of {name, value} objects
// with a get() convenience, backed by the name→value bridge map.
Object.defineProperty(pm.response, 'cookies', {
    get: function () {
        var map = {};
        if (typeof globalThis !== 'undefined' && typeof globalThis.__tropel_trp_response_cookies === 'function') {
            map = globalThis.__tropel_trp_response_cookies() || {};
        }
        var list = [];
        for (var ck in map) {
            if (map.hasOwnProperty(ck)) {
                list.push({ name: ck, value: map[ck] });
            }
        }
        list.get = function (key) {
            for (var i = 0; i < list.length; i++) {
                if (list[i].name === key) return list[i];
            }
            return undefined;
        };
        return list;
    },
    enumerable: true,
    configurable: true
});

pm.response.text = function () {
    if (typeof __tropel_trp_response_body === 'function') {
        return __tropel_trp_response_body();
    }
    return '';
};

pm.response.json = function () {
    if (typeof __tropel_trp_response_json === 'function') {
        var raw = __tropel_trp_response_json();
        if (raw) {
            return JSON.parse(raw);
        }
        throw new Error(__ns + '.response.json() — response body is not valid JSON or no response available');
    }
    throw new Error(__ns + '.response.json() is not available in this runtime');
};

pm.response.header = function (key) {
    if (typeof __tropel_trp_response_header === 'function') {
        return __tropel_trp_response_header(key);
    }
    return null;
};

// Backlog line 145: pm.response.to.be.* — Postman's chainable response
// assertions (chai-postman). Each getter/method THROWS on failure so it can
// be used inside pm.test() (the throw fails the single check).
function assertStatusClass(classCode, label) {
    var c = pm.response.code;
    var lo = classCode * 100;
    var hi = lo + 99;
    if (c < lo || c > hi) {
        throw new Error('expected response to be ' + label + ' (' + lo + '-' + hi + '), got ' + c);
    }
}

// Backlog line 41/42 (P0): the chai-postman SPECIFIC status helpers
// (notFound/unauthorized/forbidden/badRequest/accepted/rateLimited/teapot)
// and withBody used to be absent — reading them yielded `undefined` and
// pm.test recorded GREEN (silent pass) on every response. They are now
// real getters that THROW on mismatch, exactly like the status classes.
function assertStatusCode(code, label) {
    var c = pm.response.code;
    if (c !== code) {
        throw new Error('expected response to be ' + label + ' (' + code + '), got ' + c);
    }
}

pm.response.to = guardChain({
    // Backlog line 41: be/have are nested objects, so wrap them in guardChain
    // explicitly — an unknown assertion name must throw, not silently pass.
    // (Master's guardChain deliberately does not recurse: the AssertChain hot
    // path hands back `this`, so recursion would add a proxy per nested read.)
    be: guardChain({
        get success() { assertStatusClass(2, 'success'); },
        get ok() { assertStatusClass(2, 'ok'); },
        get redirection() { assertStatusClass(3, 'redirection'); },
        get clientError() { assertStatusClass(4, 'clientError'); },
        get serverError() { assertStatusClass(5, 'serverError'); },
        get error() {
            var c = pm.response.code;
            if (c < 400) {
                throw new Error('expected response to be an error (>=400), got ' + c);
            }
        },
        get info() { assertStatusClass(1, 'info'); },
        get notFound() { assertStatusCode(404, 'notFound'); },
        get unauthorized() { assertStatusCode(401, 'unauthorized'); },
        get forbidden() { assertStatusCode(403, 'forbidden'); },
        get badRequest() { assertStatusCode(400, 'badRequest'); },
        get accepted() { assertStatusCode(202, 'accepted'); },
        get rateLimited() { assertStatusCode(429, 'rateLimited'); },
        get teapot() { assertStatusCode(418, 'teapot'); },
        get withBody() {
            if (!pm.response.text()) {
                throw new Error('expected response to have a body');
            }
        },
        get withoutBody() {
            // Backlog line 41: the mirror of withBody — a non-empty body must
            // FAIL withoutBody instead of silently passing.
            if (pm.response.text()) {
                throw new Error('expected response to have no body');
            }
        },
        // Backlog line 42: chai-postman exposes .json/.html/.text as
        // PROPERTIES — Postman's own snippets emit `pm.response.to.be.json;`
        // with NO parens. As methods here, reading the property yielded a
        // truthy Function and recorded PASS on any body. Now getters: the
        // check runs on property READ (throws on mismatch, so the bare form
        // fails instead of silently passing) and the returned callable keeps
        // the paren form `to.be.json()` working.
        get json() {
            // Postman parity: to.be.json passes when the body parses as JSON.
            // Content-type is informational — a text/plain body that parses
            // still counts (Postman's chai-postman checks the body first).
            pm.response.json(); // throws on invalid JSON body
            // The getter has already validated — the callable is a no-op so
            // the paren form `to.be.json()` doesn't re-run the check (which
            // would re-read the lazy bridge and allocate a fresh function).
            return function () {};
        },
        get html() {
            var ct = String(pm.response.headers.get('content-type') || '').toLowerCase();
            if (ct.indexOf('html') === -1) {
                throw new Error('expected response to be HTML, content-type is ' + ct);
            }
            return function () {};
        },
        get text() {
            var ct = String(pm.response.headers.get('content-type') || '').toLowerCase();
            if (ct.indexOf('text') === -1 && ct.indexOf('json') === -1 && ct.indexOf('xml') === -1) {
                throw new Error('expected response to be text, content-type is ' + ct);
            }
            return function () {};
        }
    }),
    have: guardChain({
        status: function (code) {
            // Backlog line 143: pm.response.code is a VALUE now.
            // TR-114: accept BOTH a numeric code AND a reason-phrase string.
            // Postman's `to.have.status('OK')` is the canonical form; the
            // numeric form is also valid.
            var actual = pm.response.code;
            var expected = code;
            if (typeof code === 'string') {
                actual = pm.response.status;
                expected = code;
            }
            if (actual !== expected) {
                throw new Error(
                    'expected response to have status ' + code + ' but got ' + actual
                );
            }
        },
        header: function (key, value) {
            var actual = pm.response.header(key);
            if (value === undefined) {
                if (actual === null || actual === undefined) {
                    throw new Error('expected response to have header ' + key);
                }
            } else if (String(actual) !== String(value)) {
                throw new Error(
                    'expected header ' + key + ' to be ' + String(value) + ', got ' + String(actual)
                );
            }
        },
        body: function (substring) {
            var body = pm.response.text();
            if (body.indexOf(substring) === -1) {
                throw new Error('expected response body to contain ' + shortJson(substring));
            }
        },
        jsonBody: function (expected, expectedValue) {
            var body = pm.response.json();
            if (typeof expected === 'string') {
                // W1-B line 153: chai-postman treats a STRING as a KEY PATH,
                // not a deep-equal of the whole body — the old code
                // `deepEqual(body, 'key')` always threw on an object body, so
                // the canonical `pm.response.to.have.jsonBody('key')` snippet
                // was a false failure. `jsonBody('user.id')` asserts the
                // path EXISTS; `jsonBody('user.id', 7)` asserts the value at
                // that path too.
                var parts = expected.split('.');
                var node = body;
                // lodash `get` parity: only `undefined` at the FINAL segment
                // means MISSING (a present-null key like `{a: null}` passes
                // `jsonBody('a')`), but a null MID-path stops the walk — so
                // track `reached` to tell "final value is null" from
                // "stopped mid-path" (the latter is a missing path, so a
                // negated `.not.jsonBody('a.b')` on `{a:null}` must PASS).
                var reached = 0;
                for (; reached < parts.length && node !== undefined && node !== null; reached++) {
                    node = node[parts[reached]];
                }
                if (reached !== parts.length || node === undefined) {
                    throw new Error('expected response JSON body to have key ' + expected);
                }
                if (expectedValue !== undefined && !deepEqual(node, expectedValue)) {
                    throw new Error(
                        'expected ' + expected + ' to equal ' + shortJson(expectedValue) +
                        ', got ' + shortJson(node)
                    );
                }
                return;
            }
            if (!deepEqual(body, expected)) {
                throw new Error('expected response JSON body to match');
            }
        }
    })
});

// ── pm.test ──
// Backlog line 84: an async body (fn returning a Promise) used to record
// GREEN synchronously — a Promise is never `=== false`, so `pm.test(name,
// async () => { pm.expect(1).to.eql(2); })` passed before the body settled,
// and a rejected body ALSO passed (the rejection surfaced only as a side
// error). The check is now recorded at settlement time: a rejection records
// a FAILED check, matching the sync throw path below.
// NOTE: a body that fails AFTER an await (e.g. `await fetch(); pm.expect(...)`)
// settles after run_iteration drains the sample sink — it lands in the next
// iteration or is lost on the last (same class as backlog line 94, timers).
pm.test = function (name, fn) {
    try {
        var result = fn();
        if (result && typeof result.then === 'function') {
            return result.then(
                function (v) {
                    var passed = v !== false;
                    if (typeof __tropel_trp_test === 'function') {
                        __tropel_trp_test(name, passed, '');
                    }
                    return passed;
                },
                function (e) {
                    if (typeof __tropel_trp_test === 'function') {
                        // W1-A: record under the ORIGINAL name — Postman/k6
                        // record failures under the check's own name, and a
                        // CI gate on `checks{check:...}` must see the failures
                        // (renaming produced a second series that was 100%
                        // pass by construction).
                        __tropel_trp_test(name, false, '');
                    }
                    console.error(__ns + '.test error:', e);
                    return false;
                }
            );
        }
        var passed = result !== false;
        if (typeof __tropel_trp_test === 'function') {
            // 3rd arg (tags) always passed — rquickjs enforces arity, so a
            // 2-arg call against the 3-param bridge would throw (line 149).
            __tropel_trp_test(name, passed, '');
        }
        return passed;
    } catch (e) {
        if (typeof __tropel_trp_test === 'function') {
            // W1-A: see the async rejection path above — failures must be
            // recorded under the original name, not a derived ` (error)`
            // series that a check-name gate never reads.
            __tropel_trp_test(name, false, '');
        }
        console.error(__ns + '.test error:', e);
        return false;
    }
};

// Backlog line 145: pm.test.skip(name, fn) marks a test skipped WITHOUT
// running it. Skipped tests are not pass/fail checks.
pm.test.skip = function (name) {
    if (typeof __tropel_trp_test_skip === 'function') {
        __tropel_trp_test_skip(name);
    }
};

// ── pm.expect (wraps chai expect if available, else simple assert) ──
//
// Assertions THROW on failure and never auto-record a check. Postman/chai
// semantics: only `pm.test(name, fn)` records a check — wrapping an expect
// must produce exactly ONE check named by the pm.test call. Auto-recording
// here double-counted every pm.test-wrapped assertion, and embedding
// JSON.stringify(actual) in the recorded name made `pm.expect(pm.response.json())`
// stamp the ENTIRE response body as a metric tag (unbounded cardinality at
// 10 k iterations). When used inside pm.test, the throw is caught and the
// single check fails with the pm.test name. Standalone (outside pm.test)
// expects surface as a script error and do NOT produce a checks-metric
// entry — matching Postman, where check metrics come only from pm.test()
// and check().
//
// Backlog §1: unimplemented assertion PROPERTIES (e.g. `pm.expect(false)
// .to.be.true`, `pm.expect(null).to.exist`) used to read as `undefined` and
// pm.test recorded GREEN — a silent pass. Every chain object is now wrapped
// in a Proxy whose `get` trap THROWS on unknown assertion names, so a typo'd
// or unimplemented assertion fails the check instead of passing silently.
// The common chai-postman property assertions are implemented below.
// ── chai-style assertion chain (backlog line 105) ──
// pm.expect was ~10x slower than chai.expect — measured 2511 ms vs 235 ms
// over 200k assertions. EVERY call built a fresh chain object literal with
// 18 Object.defineProperty calls (addPropAssertions) AND guardChain wrapped
// each object-valued property read in a NEW Proxy. The chain is now a single
// class: getters/methods live on the prototype (defined ONCE at load), so
// each pm.expect() allocates one small instance + one proxy, and
// to/be/not/... return `this` (the same proxy) so a chain of any length
// never allocates again.
// W1-A #6: own-property + prototype walk UP TO BUT NOT INCLUDING
// Object.prototype. `prop in t` walked the ENTIRE chain, so `.toString`,
// `.constructor`, `.hasOwnProperty`, `.valueOf`, `.__proto__` all resolved
// (truthy Functions → recorded PASS) instead of throwing — a typo could
// never fail. guardChain wraps BOTH plain literals (pm.response.to — members
// are own props) AND AssertChain instances (chain getters/methods live on
// AssertChain.prototype), so the check must see own props of the target AND
// own props of its prototypes, stopping before Object.prototype. Hoisted to a
// named helper (defined once): the chain is a hot path and an inline IIFE
// would allocate a closure per property read.
function isChainMember(t, prop) {
    // `constructor` is an OWN property of every prototype object (the
    // standard back-reference), so the own-property walk below would find it
    // on AssertChain.prototype and let `pm.expect(1).to.be.constructor`
    // resolve to a truthy Function → silent PASS. It is NOT a chain member
    // — reject it like any other unknown name (W1-A #6).
    if (prop === 'constructor') {
        return false;
    }
    if (Object.prototype.hasOwnProperty.call(t, prop)) {
        return true;
    }
    var p = Object.getPrototypeOf(t);
    while (p && p !== Object.prototype) {
        if (Object.prototype.hasOwnProperty.call(p, prop)) {
            return true;
        }
        p = Object.getPrototypeOf(p);
    }
    return false;
}

function guardChain(target) {
    return new Proxy(target, {
        get: function (t, prop, receiver) {
            if (
                typeof prop === 'symbol' ||
                prop === 'then' || prop === 'toJSON' || prop === 'inspect'
            ) {
                return Reflect.get(t, prop, receiver);
            }
            if (isChainMember(t, prop)) {
                // No recursion needed: every chain getter returns `this`
                // (already the proxy), so nested reads cost one getter call.
                return Reflect.get(t, prop, receiver);
            }
            throw new Error("unknown assertion property '" + String(prop) + "'");
        }
    });
}

function AssertChain(actual, negated) {
    this._actual = actual;
    this._negated = !!negated;
}

// Chain getters: to/be/been/is/that/which/and/has/have/with/at/of/same all
// hand back the same (already guarded) chain — defined once on the prototype.
var chainGetters = ['to', 'be', 'been', 'is', 'that', 'which', 'and', 'has', 'have', 'with', 'at', 'of', 'same'];
for (var gi = 0; gi < chainGetters.length; gi++) {
    (function (name) {
        Object.defineProperty(AssertChain.prototype, name, {
            get: function () { return this; },
            enumerable: true
        });
    })(chainGetters[gi]);
}

Object.defineProperty(AssertChain.prototype, 'not', {
    get: function () {
        // One extra proxy per .not — used once per assertion, not per read.
        return guardChain(new AssertChain(this._actual, !this._negated));
    },
    enumerable: true
});

// Property assertions (getters). Each THROWS on mismatch so the enclosing
// pm.test records a failed check (a bare boolean would leave the callback's
// `undefined` statement result recorded as passed). Negation-aware via
// this._negated — installed ONCE on the prototype.
(function () {
    var checks = {
        true: function (a) { return a === true; },
        false: function (a) { return a === false; },
        null: function (a) { return a === null; },
        undefined: function (a) { return a === undefined; },
        ok: function (a) { return !!a; },
        empty: function (a) {
            if (typeof a === 'string' || Array.isArray(a)) return a.length === 0;
            if (a !== null && typeof a === 'object') return Object.keys(a).length === 0;
            return false;
        },
        exist: function (a) { return a !== null && a !== undefined; },
        NaN: function (a) { return typeof a === 'number' && isNaN(a); },
        finite: function (a) { return typeof a === 'number' && isFinite(a); }
    };
    Object.keys(checks).forEach(function (name) {
        Object.defineProperty(AssertChain.prototype, name, {
            get: function () {
                var holds = checks[name](this._actual);
                // Negated chain: the positive holding means the assertion
                // FAILS — throw when the check passes, not when it fails.
                var passed = this._negated ? !holds : holds;
                if (!passed) {
                    var label = name === 'ok' ? 'be truthy' : name === 'exist' ? 'exist' : 'be ' + name;
                    throw new Error(
                        'expected ' + shortJson(this._actual) + (this._negated ? ' not' : '') + ' to ' + label
                    );
                }
                return this;
            },
            enumerable: true
        });
    });
})();

// Value/method assertions — all negation-aware (chai parity; the old shim
// restricted the negated surface to eql/equal/be, which was a deficit).
// Every method returns `this` so chains like
// `pm.expect(x).to.be.an('string').and.to.equal('x')` work (chai parity).
AssertChain.prototype.eql = function (expected) {
    var holds = deepEqual(this._actual, expected);
    if (this._negated ? holds : !holds) {
        throw new Error(
            'expected ' + shortJson(this._actual) + (this._negated ? ' not' : '') + ' to eql ' + shortJson(expected)
        );
    }
    return this;
};
AssertChain.prototype.equal = function (expected) {
    var holds = this._actual === expected;
    if (this._negated ? holds : !holds) {
        throw new Error(
            'expected ' + shortJson(this._actual) + (this._negated ? ' not' : '') + ' to equal ' + shortJson(expected)
        );
    }
    return this;
};
AssertChain.prototype.include = function (expected) {
    // Only strings/arrays/objects can be "included into" (chai semantics).
    // indexOf WITHOUT String coercion: `include(2)` on [10, 20] must FAIL
    // (strict element membership — "10,20".indexOf("0") would pass) and
    // `include('2')` on 123 must fail too, exactly like chai.
    var obj = this._actual;
    var holds = (typeof obj === 'string' || Array.isArray(obj))
        ? obj.indexOf(expected) !== -1
        : (obj !== null && typeof obj === 'object' && expected in obj);
    if (this._negated ? holds : !holds) {
        throw new Error('expected value ' + (this._negated ? 'not ' : '') + 'to include ' + shortJson(expected));
    }
    return this;
};
AssertChain.prototype.match = function (regex) {
    var holds = regex.test(String(this._actual));
    if (this._negated ? holds : !holds) {
        throw new Error('expected value ' + (this._negated ? 'not ' : '') + 'to match ' + regex);
    }
    return this;
};
AssertChain.prototype.an = function (type) {
    var holds = typeOf(this._actual) === type;
    if (this._negated ? holds : !holds) {
        throw new Error(
            'expected value ' + (this._negated ? 'not ' : '') + 'to be an ' + type + ', got ' + typeOf(this._actual)
        );
    }
    return this;
};
AssertChain.prototype.a = function (type) {
    return this.an(type);
};
AssertChain.prototype.property = function (prop, value) {
    var obj = this._actual;
    var has = obj && (prop in obj);
    var holds = has && (value === undefined || obj[prop] === value);
    if (this._negated ? holds : !holds) {
        throw new Error('expected value ' + (this._negated ? 'not ' : '') + 'to have property ' + prop);
    }
    return this;
};
AssertChain.prototype.status = function (code) {
    // Must THROW on mismatch (Postman/chai semantics) — a boolean return
    // makes `pm.test` treat the callback's `undefined` statement result as
    // passed. Backlog line 143: pm.response.code is a VALUE now.
    // TR-114: accept BOTH a numeric code AND a reason-phrase string
    // (`to.have.status('OK')` is Postman's canonical form).
    var actual = pm.response.code;
    if (typeof code === 'string') {
        actual = pm.response.status;
    }
    var holds = actual === code;
    if (this._negated ? holds : !holds) {
        throw new Error('expected response ' + (this._negated ? 'not ' : '') + 'to have status ' + code + ' but got ' + actual);
    }
    return this;
};
AssertChain.prototype.header = function (key, value) {
    var header = pm.response.header(key);
    var holds = header === value;
    if (this._negated ? holds : !holds) {
        throw new Error('expected header ' + key + ' ' + (this._negated ? 'not ' : '') + 'to be ' + shortJson(value) + ', got ' + shortJson(header));
    }
    return this;
};
AssertChain.prototype.jsonBody = function (expected) {
    var body = pm.response.json();
    var holds = deepEqual(body, expected);
    if (this._negated ? holds : !holds) {
        throw new Error('expected response body ' + (this._negated ? 'not ' : '') + 'to match');
    }
    return this;
};

pm.expect = function (actual) {
    // W1-B: delegate to chai-shim's Assertion when it is loaded — its
    // surface covers below/lengthOf/oneOf/above/least/most/lessThan/
    // instanceOf/throw/keys/contain/members/closeTo/within and deep-aware
    // include, which 6 of Postman's 17 stock snippets need (they failed
    // with "unknown assertion property" against the AssertChain surface).
    // chai-shim loads in the runtime bundle BEFORE any user script runs, so
    // at call time it is always present there; standalone pm.js (driver
    // tests, hosts that eval pm.js alone) falls back to AssertChain.
    if (typeof chai !== 'undefined' && chai.expect) {
        return chai.expect(actual);
    }
    // One small instance + one proxy per call (backlog line 105). The whole
    // chain surface lives on AssertChain.prototype; `to`/`be`/`not` return
    // `this` (the proxy), so the unknown-name guard covers every position
    // and a chain of any length allocates nothing more.
    return guardChain(new AssertChain(actual, false));
};

// ── pm.request ──
// Backlog line 145: prerequest scripts could not add an auth header or sign a
// request because pm.request didn't exist AND the runner rebuilt the wire
// request from the static collection item, discarding mutations. The runner
// now reads PmState.request (seeded from item.request before prerequest) when
// building the outgoing request, so mutations made here go out on the wire.
// Live getters/setters delegate to the __tropel_trp_request_* bridges
// (registered lazily, like exec.js).
pm.request = {};

Object.defineProperty(pm.request, 'url', {
    get: function () {
        if (typeof __tropel_trp_request_url === 'function') return __tropel_trp_request_url();
        return '';
    },
    set: function (url) {
        if (typeof __tropel_trp_request_url_set === 'function') __tropel_trp_request_url_set(String(url));
    },
    enumerable: true,
    configurable: true
});

Object.defineProperty(pm.request, 'method', {
    get: function () {
        if (typeof __tropel_trp_request_method === 'function') return __tropel_trp_request_method();
        return 'GET';
    },
    set: function (method) {
        if (typeof __tropel_trp_request_method_set === 'function') __tropel_trp_request_method_set(String(method));
    },
    enumerable: true,
    configurable: true
});

// Postman's pm.request.headers is a HeaderList: .add({key,value}) is THE
// canonical prerequest idiom for attaching an Authorization header.
pm.request.headers = {
    add: function (header) {
        if (!header || header.key === undefined || header.key === null) return;
        if (typeof __tropel_trp_request_header_set === 'function') {
            __tropel_trp_request_header_set(String(header.key), header.value == null ? '' : String(header.value));
        }
    },
    upsert: function (header) {
        pm.request.headers.add(header);
    },
    get: function (key) {
        if (typeof __tropel_trp_request_header_get === 'function') {
            var v = __tropel_trp_request_header_get(key);
            return v !== null && v !== undefined ? v : undefined;
        }
        return undefined;
    },
    remove: function (key) {
        if (typeof __tropel_trp_request_header_unset === 'function') {
            __tropel_trp_request_header_unset(key);
        }
    },
    all: function () {
        if (typeof __tropel_trp_request_headers === 'function') {
            var map = __tropel_trp_request_headers() || {};
            var arr = [];
            for (var k in map) {
                if (map.hasOwnProperty(k)) arr.push({ key: k, value: map[k] });
            }
            return arr;
        }
        return [];
    },
    each: function (cb) {
        var all = pm.request.headers.all();
        for (var i = 0; i < all.length; i++) cb(all[i]);
    },
    toObject: function () {
        if (typeof __tropel_trp_request_headers === 'function') {
            return __tropel_trp_request_headers() || {};
        }
        return {};
    }
};

// pm.request.body — raw text form backed by the request body bridge.
// Postman's canonical idiom is `pm.request.body.raw = '...'`, which requires
// a STABLE object whose `raw` accessor is bridge-wired (a getter returning a
// fresh object each read would silently swallow the mutation).
// Backlog line 101: `mode` was a plain module-scope property — a fresh
// request per iteration re-seeded the raw text but NOT the mode, so
// `pm.request.body.mode` leaked the previous iteration's value. It is now
// a live getter backed by __tropel_trp_request_body_mode (falling back to
// the last-assigned value when the bridge is absent, e.g. test stubs).
var _pmRequestBody = {};
var _pmBodyModeFallback = 'raw';
Object.defineProperty(_pmRequestBody, 'mode', {
    get: function () {
        if (typeof __tropel_trp_request_body_mode === 'function') {
            var m = __tropel_trp_request_body_mode();
            if (m) return m;
        }
        return _pmBodyModeFallback;
    },
    set: function (m) { _pmBodyModeFallback = m || 'raw'; },
    enumerable: true,
    configurable: true
});
Object.defineProperty(_pmRequestBody, 'raw', {
    get: function () {
        if (typeof __tropel_trp_request_body === 'function') {
            var b = __tropel_trp_request_body();
            if (b !== null && b !== undefined) return b;
        }
        return '';
    },
    set: function (raw) {
        if (typeof __tropel_trp_request_body_set === 'function') {
            __tropel_trp_request_body_set(raw == null ? '' : String(raw));
        }
    },
    enumerable: true,
    configurable: true
});
Object.defineProperty(pm.request, 'body', {
    get: function () { return _pmRequestBody; },
    set: function (body) {
        // Accept a plain string OR an object {mode, raw} — both are common
        // Postman spellings.
        var raw = body == null ? '' : (typeof body === 'object' ? (body.raw != null ? String(body.raw) : '') : String(body));
        if (typeof __tropel_trp_request_body_set === 'function') {
            __tropel_trp_request_body_set(raw);
        }
        _pmRequestBody.mode = body && body.mode ? body.mode : 'raw';
    },
    enumerable: true,
    configurable: true
});

// pm.request.auth — accepts the tagged AuthConfig JSON shape so a prerequest
// script can sign the outgoing request (the primary purpose of pm.request).
// Backed by a stored copy so reads echo the last-set config (a collection
// may inspect it in a test script).
var _pmRequestAuth = null;
Object.defineProperty(pm.request, 'auth', {
    // Backlog line 101: the getter was a module-scope singleton, so a
    // request with NO auth (or a different auth) on the next iteration
    // still read the previous iteration's value. Read LIVE from the current
    // request's auth via __tropel_trp_request_auth; fall back to the stored
    // copy only when the bridge is absent (test stubs / browser slice).
    get: function () {
        if (typeof __tropel_trp_request_auth === 'function') {
            var j = __tropel_trp_request_auth();
            if (j) {
                try { return JSON.parse(j); } catch (e) { return null; }
            }
            return null;
        }
        return _pmRequestAuth;
    },
    set: function (auth) {
        _pmRequestAuth = auth;
        // JSON.stringify(undefined) is not a string — skip the bridge (and
        // the Rust parse) when clearing auth; use {type:'noauth'} to clear
        // explicitly, exactly like Postman.
        if (auth === undefined || auth === null) return;
        if (typeof __tropel_trp_request_auth_set === 'function') {
            __tropel_trp_request_auth_set(JSON.stringify(auth));
        }
    },
    enumerable: true,
    configurable: true
});

// ── pm.cookies ──
// Backlog line 145: Postman's pm.cookies reads the cookie jar for the current
// domain. In a headless load runner the closest proxy is the response's
// Set-Cookie map (__tropel_trp_response_cookies).
pm.cookies = {
    get: function (name) {
        var jar = pm.cookies.toObject();
        return jar[name];
    },
    has: function (name) {
        return pm.cookies.toObject().hasOwnProperty(name);
    },
    toObject: function () {
        if (typeof __tropel_trp_response_cookies === 'function') {
            return __tropel_trp_response_cookies() || {};
        }
        return {};
    },
    list: function () {
        var jar = pm.cookies.toObject();
        var arr = [];
        for (var k in jar) {
            if (jar.hasOwnProperty(k)) arr.push({ name: k, value: jar[k] });
        }
        return arr;
    }
};

// Backlog line 145: pm.expect.fail — chai's static fail: always throws.
// Used by collections to force a failure inside a pm.test block.
pm.expect.fail = function (message) {
    throw new Error(message || 'expect.fail() called');
};

// Truncate a value for ERROR MESSAGES only (never for metric tags). Keeps
// failure logs readable when the actual value is a large response body.
function shortJson(v) {
    var s;
    try {
        s = JSON.stringify(v);
    } catch (e) {
        s = String(v);
    }
    if (s && s.length > 120) {
        return s.slice(0, 117) + '...';
    }
    return s;
}

// Proper JS deep-equality (chai .eql semantics): handles NaN, undefined,
// key order, nested arrays/objects. W2 line 190: the single canonical
// implementation now lives in js/shared/deep-equal.js
// (globalThis.__tropelDeepEqual), evaluated FIRST in every bundle. This
// thin wrapper keeps pm.js's internal call sites (jsonPath, to.have.jsonBody,
// pm.expect AssertChain) working — and, because chai-shim and lodash-shim
// delegate to the SAME function, a fix here can no longer skew the others.
function deepEqual(a, b, seen) {
    return globalThis.__tropelDeepEqual(a, b, seen);
}

// Backlog line 88: chai's .include semantics — substring for strings,
// element membership (strict indexOf) for arrays, KEY membership for
// objects. The old String(container).indexOf(...) made [11,22].include(1)
// and {a:1}.include('object') pass.
function includesValue(container, value) {
    if (typeof container === 'string') return container.indexOf(value) !== -1;
    if (Array.isArray(container)) return container.indexOf(value) !== -1;
    if (container !== null && typeof container === 'object') return value in container;
    return false;
}

// ── pm.iterationData ──
pm.iterationData = {
    get: function (key) {
        if (typeof __tropel_trp_iteration_data_get === 'function') {
            var raw = __tropel_trp_iteration_data_get(key);
            if (raw === null || raw === undefined) return null;
            // Values come JSON-encoded from the bridge — parse to restore type
            try { return JSON.parse(raw); }
            catch (e) { return raw; }
        }
        return null;
    }
};

// Chai-style type names: 'array', 'object', 'string', 'number', 'boolean',
// 'null', 'undefined', 'function'.
function typeOf(v) {
    if (v === null) return 'null';
    if (Array.isArray(v)) return 'array';
    return typeof v;
}

function buildMultipartBody(formdata) {
    var boundary = '----TropelFormBoundary' + Math.random().toString(36).slice(2);
    var parts = [];
    for (var i = 0; i < formdata.length; i++) {
        var fp = formdata[i];
        if (!fp || !fp.key) continue;
        var value = fp.value == null ? '' : fp.value;
        if (typeof value !== 'string') {
            try {
                value = JSON.stringify(value);
            } catch (e) {
                value = String(value);
            }
        }
        parts.push('--' + boundary + '\r\n');
        parts.push('Content-Disposition: form-data; name="' + escapeMultipartFieldName(fp.key) + '"\r\n\r\n');
        parts.push(value + '\r\n');
    }
    parts.push('--' + boundary + '--\r\n');
    return {
        body: parts.join(''),
        contentType: 'multipart/form-data; boundary=' + boundary
    };
}

function escapeMultipartFieldName(name) {
    return String(name).replace(/\\/g, '\\\\').replace(/"/g, '\\"');
}

// ── pm.sendRequest (for chaining requests within a test) ──
// Supports the auth-token-fetch pattern: send a request to obtain
// an auth token, then store it via pm.variables.set().
// Handles both Postman-style options and simple string URLs.
pm.sendRequest = function (options, callback) {
    // Delegate to native implementation
    if (typeof __tropel_trp_send_request === 'function') {
        // Normalize options
        var url = '';
        var method = 'GET';
        var headers = {};
        var body = '';
        var timeout = 30000; // 30s default

        if (typeof options === 'string') {
            // Simple string URL
            url = options;
        } else if (options && typeof options === 'object') {
            // Postman-style request object
            url = options.url || '';
            method = options.method || 'GET';
            timeout = options.timeout || 30000;

            // Handle Postman-style headers: array of {key, value} or plain object
            if (options.header && Array.isArray(options.header)) {
                // Postman array format: [{key: "Content-Type", value: "application/json"}]
                headers = {};
                for (var i = 0; i < options.header.length; i++) {
                    var h = options.header[i];
                    if (h && h.key) {
                        headers[h.key] = h.value !== undefined ? h.value : '';
                    }
                }
            } else if (options.headers) {
                // Plain object or Postman header object — COPY it: the
                // formdata branch below stamps Content-Type with the
                // generated boundary, and mutating the caller's object
                // (options are often hoisted to module scope) would leak
                // state across iterations.
                headers = {};
                if (!Array.isArray(options.headers)) {
                    for (var hk in options.headers) {
                        if (options.headers.hasOwnProperty(hk)) headers[hk] = options.headers[hk];
                    }
                }
            }

            // Handle Postman-style body
            if (options.body) {
                if (typeof options.body === 'string') {
                    body = options.body;
                } else if (options.body.mode) {
                    // Postman body object: {mode: "raw", raw: "..."}
                    switch (options.body.mode) {
                        case 'raw':
                            body = options.body.raw || '';
                            break;
                        case 'urlencoded':
                            if (options.body.urlencoded && Array.isArray(options.body.urlencoded)) {
                                var pairs = [];
                                for (var j = 0; j < options.body.urlencoded.length; j++) {
                                    var param = options.body.urlencoded[j];
                                    if (param && param.key) {
                                        pairs.push(encodeURIComponent(param.key) + '=' + encodeURIComponent(param.value || ''));
                                    }
                                }
                                body = pairs.join('&');
                            }
                            break;
                        case 'formdata':
                            if (options.body.formdata && Array.isArray(options.body.formdata)) {
                                var multipart = buildMultipartBody(options.body.formdata);
                                body = multipart.body;
                                // ALWAYS stamp the generated boundary — the
                                // old `!headers['Content-Type']` guard was
                                // false whenever the caller declared
                                // multipart/form-data, so the boundary never
                                // reached the request. `headers` is a copy of
                                // options.headers (never the caller's object).
                                // Drop any lowercase variant so only ONE
                                // Content-Type (with the boundary) is sent.
                                delete headers['content-type'];
                                headers['Content-Type'] = multipart.contentType;
                            }
                            break;
                        case 'graphql':
                            if (options.body.query) {
                                body = JSON.stringify({query: options.body.query, variables: options.body.variables || {}});
                            }
                            break;
                        default:
                            body = options.body.raw || JSON.stringify(options.body);
                    }
                } else {
                    // Plain object body — JSON encode
                    try {
                        body = JSON.stringify(options.body);
                    } catch (e) {
                        body = String(options.body);
                    }
                }
            }
        }

        var resultJson = __tropel_trp_send_request(
            method.toUpperCase(),
            url,
            JSON.stringify(headers),
            typeof body === 'string' ? body : JSON.stringify(body),
            timeout,
            // k6-style responseType — Postman sendRequest has no such field,
            // default to "text" (bridge requires the 6th arg)
            (options && options.responseType) || 'text'
        );

        // Fire callback with the response. The callback is invoked OUTSIDE
        // the try: the old code called it INSIDE, so a throw from the user's
        // callback (a failing pm.expect — the entire point of sendRequest)
        // was caught by the sibling catch and the callback re-entered with a
        // bogus "Failed to parse" error — running twice and replacing the
        // real error (W1-B line 149). Parse/build here; call the user's code
        // only once, after, so its exceptions propagate as-is.
        if (typeof callback === 'function') {
            var cbErr = null;
            var response = null;
            try {
                var result = JSON.parse(resultJson);
                // Backlog line 147: transport failures (DNS/conn refused/timeout)
                // used to arrive as callback(null, {code: 0}) — a "success" to
                // the universal `if (err)` guard, so auth-token-fetch retry logic
                // never fired. The bridge now stamps an `error` field; surface it
                // as the first (err) argument so the canonical guard works.
                if (result.error) {
                    cbErr = new Error(result.error);
                } else {
                    response = {
                        code: result.code || 0,
                        status: result.statusText || '',
                        text: function () { return result.body || ''; },
                        json: function () {
                            try { return JSON.parse(result.body || '{}'); }
                            catch (e) { return null; }
                        },
                        headers: function () { return result.headers || {}; },
                        responseTime: result.responseTime || 0
                    };
                }
            } catch (e) {
                cbErr = new Error('Failed to parse sendRequest response: ' + e.message);
            }
            if (cbErr) {
                callback(cbErr, null);
            } else {
                callback(null, response);
            }
        }
        return;
    }

    // No native function available - throw a clear error
    throw new Error(__ns + '.sendRequest is not available in this runtime (native __tropel_trp_send_request not found)');
};

// ── pm.execution ──
// Backlog line 145: postman.setNextRequest is the LEGACY flow-control global
// that nearly all real collections use (pm.execution.setNextRequest is the
// newer spelling). Both delegate to the same bridge.
var postman = postman || {};
postman.setNextRequest = function (requestName) {
    pm.execution.setNextRequest(requestName);
};

pm.execution = {
    setNextRequest: function (requestName) {
        if (typeof __tropel_trp_set_next_request === 'function') {
            __tropel_trp_set_next_request(requestName);
        }
    },
    skipRequest: function () {
        // Backlog line 146: skipRequest must skip ONLY the current request
        // and move to the next item. Routing it through setNextRequest(null)
        // (a) threw — null into a strict String param — and (b) inherited
        // setNextRequest's "stop the whole run" semantics. Use the dedicated
        // __tropel_trp_skip_request bridge instead.
        if (typeof __tropel_trp_skip_request === 'function') {
            __tropel_trp_skip_request();
        }
    }
    // W1-A: stopOnError deliberately ABSENT — Postman has no such method
    // (only setNextRequest/skipRequest), and the old wired no-op set a
    // skip_tests flag that nothing ever read. An invented method on a dead
    // flag silently ignored the author's intent; being absent makes the
    // call throw "is not a function", surfacing the error like real Postman.
};

// ── pm.info (live, backlog line 101) ──
// Was a hardcoded stub (eventName 'test', iteration 0, iterationCount 1,
// requestName '', requestId ''). Each field is now a getter backed by the
// __tropel_trp_info bridge, so a test script sees the real iteration,
// request name, and configured iteration count. Falls back to the old
// stub values when the bridge is absent (test stubs / browser slice).
var _pmInfoFallback = {
    eventName: 'test',
    iteration: 0,
    iterationCount: 1,
    requestName: '',
    requestId: ''
};
function _pmInfoRead() {
    if (typeof __tropel_trp_info === 'function') {
        var raw = __tropel_trp_info();
        if (raw) {
            try { return JSON.parse(raw); } catch (e) { /* fall through */ }
        }
    }
    return _pmInfoFallback;
}
pm.info = {};
['eventName', 'iteration', 'iterationCount', 'requestName', 'requestId'].forEach(function (k) {
    Object.defineProperty(pm.info, k, {
        get: function () { return _pmInfoRead()[k]; },
        enumerable: true,
        configurable: true
    });
});

// ── pm.metrics (custom metrics) ──
pm.metrics = {
    // Add a value to a custom metric (creates it if it doesn't exist).
    // Metric types: 'counter', 'gauge', 'rate', 'trend' (default: 'trend')
    add: function (name, value, metricType) {
        if (typeof __tropel_trp_metrics_add === 'function') {
            var type = metricType || 'trend';
            __tropel_trp_metrics_add(name, Number(value), type);
        }
    },
    // Get the current value of a custom metric.
    get: function (name) {
        if (typeof __tropel_trp_metrics_get === 'function') {
            return __tropel_trp_metrics_get(name);
        }
        return null;
    },
    // Convenience: add a counter value (always increments by the value).
    counter: function (name, value) {
        pm.metrics.add(name, value, 'counter');
    },
    // Convenience: set a gauge value (records the current value).
    gauge: function (name, value) {
        pm.metrics.add(name, value, 'gauge');
    },
    // Convenience: add a rate event (value = 1.0 for success, 0.0 for failure).
    rate: function (name, value) {
        pm.metrics.add(name, value, 'rate');
    },
    // Convenience: add a trend sample (records the value for percentile analysis).
    trend: function (name, value) {
        pm.metrics.add(name, value, 'trend');
    }
};

// ── k6 builtins moved out ────────────────────────────────────────────────
// `check`, `group`, `Counter`, `Gauge`, `Rate` and `Trend` used to be
// defined here and installed onto globalThis from this file. They are k6's
// API, not Postman's, and keeping them here forced every non-Postman format
// to load the whole 70 KB Postman shim just to get `check()` (TR-501).
// They now live in js/shared/k6-core.js, which every bundle includes.

    // ── pm.visualizer ── (stayed with pm: it is Postman surface, and it
    // needs `pm` to exist. The k6-core extraction swept it up by accident
    // because it sits between `check` and the metric constructors.)
    pm.visualizer = {
        set: function (template, data) {
            // Visualizer is not supported in CLI mode
            console.log('[visualizer] template:', template, 'data:', data);
        }
    };

    // Expose the Postman globals. The k6-style globals (check/group/Counter/
    // Gauge/Rate/Trend) are installed by k6-core.js, which loads first.
    if (typeof globalThis !== 'undefined') {
        globalThis.postman = postman;
    }
    return pm;
}

// ── Install: pm (frozen Postman-compat peer view) + canonical + aliases ──
// The canonical name and its aliases come from the host-set global
// `__tropel_sandbox_config` (written by SandboxConfig::render_js_preamble
// before this bundle evals — P4b). Absent a config, the stock install
// applies: canonical `trp`, NO default aliases (Postman convention — one
// canonical namespace; aliases are opt-in per embedder).
//
// Scoped in an IIFE so the helper vars don't leak onto globalThis (pm.js is
// non-strict, so a top-level `var` becomes a global — and a leaked
// `canonical` could collide with an embedder's chosen namespace). Only `pm`
// and `__tropel_build_binding` remain global, exactly as before.
var pm = __tropel_build_binding('pm');
(function () {
    var cfg = (typeof __tropel_sandbox_config === 'object' && __tropel_sandbox_config) || {};
    var namespace = (typeof cfg.namespace === 'string' && cfg.namespace.length > 0)
        ? cfg.namespace
        : 'trp';
    var aliases = Array.isArray(cfg.aliases) ? cfg.aliases : [];
    // P-A: when namespace === 'pm', reuse the existing pm object instead of
    // building a second identical object graph (~28KB/VU, ~200 closures).
    var canonical = (namespace === 'pm') ? pm : __tropel_build_binding(namespace);
    // True alias — identical object, one line, not a proxy (P4b).
    var install = [['pm', pm]];
    // `pm` is the frozen Postman-compat peer view — it owns its name. A
    // namespace or alias of 'pm' would collide with the already-installed
    // non-configurable binding and silently fail, so it is skipped.
    if (namespace !== 'pm') {
        install.push([namespace, canonical]);
    }
    for (var i = 0; i < aliases.length; i++) {
        var alias = aliases[i];
        if (typeof alias === 'string' && alias.length > 0
            && alias !== namespace && alias !== 'pm') {
            install.push([alias, canonical]);
        }
    }
    var g = typeof globalThis !== 'undefined' ? globalThis : null;
    if (g) {
        install.forEach(function (entry) {
            try {
                Object.defineProperty(g, entry[0], {
                    value: entry[1], writable: false, configurable: false
                });
            } catch (e) {
                // Tolerate double eval in the same context: the binding is
                // already installed read-only, and the first eval's bindings
                // win (the `var pm` above no-ops against the non-writable
                // global).
            }
        });
    }
})();

// ── Export for module systems ──
if (typeof module !== 'undefined' && module.exports) {
    module.exports = pm;
}
