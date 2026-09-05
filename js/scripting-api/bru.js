// ── bru.* API for Tropel ───────────────────────────────────────────────────
// Bruno-compat peer view over the shared runtime state.
// Bruno's API shape: `bru.getEnvVar()`, `req.setHeader()`, `res.getBody()`
// (three objects rather than one namespace — P4b). Frozen compat: Bruno.
// Uses the same __tropel_trp_* bridges as the pm binding; the state model
// is binding-agnostic (P4b core).
//
// Frozen compat: this layer reproduces Bruno's documented scripting API
// and does NOT gain features from trp.* or pm.*. See TROPEL_MODULARIZATION_TODO.md
// §P4b: "compatibility layers must be frozen, not co-evolved."

(function () {
    var g = typeof globalThis !== 'undefined' ? globalThis : null;
    if (!g) return;

    // ── bru namespace ──────────────────────────────────────────────────────
    var bru = {};

    // Environment
    bru.getEnvVar = function (key) {
        if (typeof __tropel_trp_environment_get === 'function') {
            // W2 line 182: the bridge returns the value JSON-encoded ("..."
            // with literal quotes) so the correct JS type round-trips — the
            // old raw return meant getEnvVar('baseUrl') came back WITH the
            // quotes and every URL built from it was malformed. Parse like
            // pm.environment.get (pm.js:26).
            var raw = __tropel_trp_environment_get(key);
            if (raw === null || raw === undefined) return null;
            try { return JSON.parse(raw); } catch (e) { return raw; }
        }
        return null;
    };
    // The three setters below MUST JSON-encode, because their getters
    // JSON.parse. `String(value)` made set/get non-inverse: setVar('id','1234')
    // read back as the NUMBER 1234, and setVar('u',{id:7}) read back as the
    // string "[object Object]" — a collection doing
    // `bru.setVar('p', res.getBody()); req.setBody(bru.getVar('p'))` put that
    // literal string on the wire and still ran green. pm.js has always
    // stringified here; this is the sibling that was missed.
    function encodeBruValue(value) {
        if (value === undefined) return '';
        try {
            return JSON.stringify(value);
        } catch (e) {
            return String(value);
        }
    }
    bru.setEnvVar = function (key, value) {
        if (typeof __tropel_trp_environment_set === 'function') {
            __tropel_trp_environment_set(key, encodeBruValue(value));
        }
    };

    // Runtime variables — Bruno's bru.getVar/setVar are RUNTIME-scope
    // (in-memory, per collection run), NOT collection scope. TROPEL_PARITY_BRUNO.md
    // §2: they used to map to the collection_vars bridges, so a runtime var set
    // by one request could not be read by the next (the core request-chaining
    // idiom silently broke). They now route through the pm.variables store
    // (__tropel_trp_variables_*, the same fall-through lookup pm.variables uses).
    bru.getVar = function (key) {
        if (typeof __tropel_trp_variables_get === 'function') {
            var raw = __tropel_trp_variables_get(key);
            if (raw === null || raw === undefined) return undefined;
            try { return JSON.parse(raw); } catch (e) { return raw; }
        }
        return undefined;
    };
    bru.setVar = function (key, value) {
        if (typeof __tropel_trp_variables_set === 'function') {
            __tropel_trp_variables_set(key, encodeBruValue(value));
        }
    };
    bru.hasVar = function (key) {
        // Single bridge round-trip: getVar returns undefined on a miss.
        var v = bru.getVar(key);
        return v !== undefined;
    };
    bru.deleteVar = function (key) {
        if (typeof __tropel_trp_variables_unset === 'function') {
            __tropel_trp_variables_unset(key);
        }
    };
    // NOTE (deleteAllVars cascade): deleteVar → variables_unset removes from
    // local + collection + environment + globals, so on a key collision this
    // also clears an env/global var of the same name. A scoped runtime-only
    // unset needs a bridge change (TROPEL_PARITY_BRUNO.md §7).
    bru.deleteAllVars = function () {
        var all = bru.getAllVars();
        for (var k in all) {
            if (Object.prototype.hasOwnProperty.call(all, k)) bru.deleteVar(k);
        }
    };
    // getAllVars: reads the LOCAL runtime store that setVar writes
    // (__tropel_trp_variables_to_object). W2 line 182: it used to read
    // collection_vars while setVar wrote local_vars — the aliasing comment
    // claimed the stores alias at the Rust level; they don't, so a runtime
    // var set via setVar never appeared in getAllVars.
    bru.getAllVars = function () {
        if (typeof __tropel_trp_variables_to_object === 'function') {
            var map = __tropel_trp_variables_to_object() || {};
            var out = {};
            for (var k in map) {
                if (Object.prototype.hasOwnProperty.call(map, k)) {
                    var raw = map[k];
                    try { out[k] = JSON.parse(raw); } catch (e) { out[k] = raw; }
                }
            }
            return out;
        }
        return {};
    };

    // Explicit collection-scope accessors (TROPEL_PARITY_BRUNO.md §2). Bruno
    // distinguishes bru.getVar/setVar (RUNTIME scope) from the collection
    // scope — getCollectionVar/setCollectionVar/hasCollectionVar/delete* map
    // to the __tropel_trp_collection_vars_* bridges, independent of the
    // runtime store used by getVar/setVar.
    bru.getCollectionVar = function (key) {
        if (typeof __tropel_trp_collection_vars_get === 'function') {
            var raw = __tropel_trp_collection_vars_get(key);
            if (raw === null || raw === undefined) return undefined;
            try { return JSON.parse(raw); } catch (e) { return raw; }
        }
        return undefined;
    };
    bru.setCollectionVar = function (key, value) {
        if (typeof __tropel_trp_collection_vars_set === 'function') {
            __tropel_trp_collection_vars_set(key, encodeBruValue(value));
        }
    };
    bru.hasCollectionVar = function (key) {
        if (typeof __tropel_trp_collection_vars_has === 'function') {
            return __tropel_trp_collection_vars_has(key);
        }
        return false;
    };
    bru.deleteCollectionVar = function (key) {
        if (typeof __tropel_trp_collection_vars_unset === 'function') {
            __tropel_trp_collection_vars_unset(key);
        }
    };
    bru.deleteAllCollectionVars = function () {
        if (typeof __tropel_trp_collection_vars_to_object !== 'function' ||
            typeof __tropel_trp_collection_vars_unset !== 'function') {
            return;
        }
        var map = __tropel_trp_collection_vars_to_object() || {};
        for (var k in map) {
            if (Object.prototype.hasOwnProperty.call(map, k)) {
                __tropel_trp_collection_vars_unset(k);
            }
        }
    };

    // Request body
    bru.getReqBody = function () {
        if (typeof __tropel_trp_request_body === 'function') {
            return __tropel_trp_request_body();
        }
        return null;
    };
    bru.setReqBody = function (body) {
        if (typeof __tropel_trp_request_body_set === 'function') {
            __tropel_trp_request_body_set(body === undefined ? '' : String(body));
        }
    };

    // Request headers
    bru.getReqHeader = function (name) {
        if (typeof __tropel_trp_request_header_get === 'function') {
            return __tropel_trp_request_header_get(name);
        }
        return null;
    };
    bru.setReqHeader = function (name, value) {
        if (typeof __tropel_trp_request_header_set === 'function') {
            __tropel_trp_request_header_set(name, String(value));
        }
    };

    // Response
    bru.getResBody = function () {
        if (typeof __tropel_trp_response_body === 'function') {
            return __tropel_trp_response_body();
        }
        return null;
    };
    bru.getResHeader = function (name) {
        if (typeof __tropel_trp_response_header === 'function') {
            return __tropel_trp_response_header(name);
        }
        return null;
    };
    bru.getResStatus = function () {
        if (typeof __tropel_trp_response_code === 'function') {
            return __tropel_trp_response_code();
        }
        return 0;
    };
    bru.getResTime = function () {
        if (typeof __tropel_trp_response_time === 'function') {
            return __tropel_trp_response_time();
        }
        return 0;
    };

    // Environment name (stub — tropel doesn't track the env name independently)
    bru.getEnvName = function () {
        return null;
    };

    // Assertion — Bruno's bru.assert(expression, errorMessage) evaluates the
    // expression string as code, matching user expectations.
    bru.assert = function (expr, errorMessage) {
        var passed = false;
        if (typeof expr === 'function') {
            passed = expr();
        } else if (typeof expr === 'string') {
            try { passed = eval(expr); } catch (e) { passed = false; }
        } else {
            passed = !!expr;
        }
        if (typeof __tropel_trp_test === 'function') {
            // W2 line 182: the bridge takes (name, passed: BOOL, tags) — an
            // int 1/0 has NO bool coercion in rquickjs (pm.js:506-508 warns
            // about exactly this rule), so `passed ? 1 : 0` threw on EVERY
            // call. Pass a real bool + the empty tags string like pm.test.
            __tropel_trp_test(
                'bru.assert: ' + (errorMessage || String(expr)),
                passed ? true : false,
                ''
            );
        }
    };

    // Sleep (delegates to the native sleep bridge, in ms).
    // NOTE: Bruno's bru.sleep(ms) is async (returns a Promise); the
    // synchronous implementation is a QuickJS embedding limitation.
    bru.sleep = function (ms) {
        if (typeof __tropel_native_sleep === 'function' && typeof ms === 'number' && ms > 0) {
            __tropel_native_sleep(ms);
        }
    };

    // Logging
    bru.log = function () {
        if (typeof console !== 'undefined' && typeof console.log === 'function') {
            console.log.apply(console, arguments);
        }
    };

    // Flow control — Bruno's bru.next() maps to setNextRequest
    bru.next = function (requestName) {
        if (typeof __tropel_trp_set_next_request === 'function') {
            __tropel_trp_set_next_request(requestName);
        }
    };

    // ── bru.runRequest (TR-474) ─────────────────────────────────────────────
    //
    // Runs ANOTHER request from the caller's collection and returns its
    // response. The agent cannot do this itself: the collection, its auth and
    // its variables live in the API client, not here. So this is the one
    // binding that calls BACK out of the realm, over the host-callback
    // channel the caller opened with a `runId`.
    //
    // Guarded like every other bridge here. Without the channel the binding
    // is absent and this REFUSES BY NAME — a load run has no collection to
    // re-enter, and silently returning undefined there would read as "the
    // request ran and gave nothing back".
    bru.runRequest = function (path) {
        if (typeof __tropel_trp_run_request !== 'function') {
            throw new Error(
                'bru.runRequest is not available here: it re-enters the API client\'s ' +
                'collection, which this runtime has no access to. It works when the ' +
                'caller opened a host-callback channel (tropel TR-474).'
            );
        }
        var raw = __tropel_trp_run_request(String(path));
        var parsed;
        try {
            parsed = JSON.parse(raw);
        } catch (e) {
            throw new Error('bru.runRequest: the host answered with invalid JSON: ' + raw);
        }
        // A refusal from the host stays a THROW, not a value. The recursion
        // guard, an unknown name and a timeout all arrive this way, and a
        // script must not be able to mistake any of them for a response.
        if (parsed && parsed.error) {
            throw new Error('bru.runRequest: ' + parsed.error);
        }
        return parsed;
    };

    // ── req object (pre-request scripts) ───────────────────────────────────
    var req = {};
    req.setHeader = function (name, value) {
        if (typeof __tropel_trp_request_header_set === 'function') {
            __tropel_trp_request_header_set(name, String(value));
        }
    };
    req.setMethod = function (method) {
        if (typeof __tropel_trp_request_method_set === 'function') {
            __tropel_trp_request_method_set(method);
        }
    };
    req.setUrl = function (url) {
        if (typeof __tropel_trp_request_url_set === 'function') {
            __tropel_trp_request_url_set(url);
        }
    };
    req.setBody = function (body) {
        if (typeof __tropel_trp_request_body_set === 'function') {
            __tropel_trp_request_body_set(body === undefined ? '' : String(body));
        }
    };
    req.getHeader = function (name) {
        if (typeof __tropel_trp_request_header_get === 'function') {
            return __tropel_trp_request_header_get(name);
        }
        return null;
    };
    req.getBody = function () {
        if (typeof __tropel_trp_request_body === 'function') {
            return __tropel_trp_request_body();
        }
        return null;
    };

    // ── res object (test scripts) ──────────────────────────────────────────
    var res = {};
    res.getBody = function () {
        if (typeof __tropel_trp_response_body === 'function') {
            return __tropel_trp_response_body();
        }
        return null;
    };
    res.getHeader = function (name) {
        if (typeof __tropel_trp_response_header === 'function') {
            return __tropel_trp_response_header(name);
        }
        return null;
    };
    // Bruno's res.getStatus() returns the numeric status CODE; res.getStatusText()
    // returns the text (e.g. "OK"). TROPEL_PARITY_BRUNO.md §0: the old
    // implementation returned the text from getStatus() (a silent failure for
    // the canonical `expect(res.getStatus()).to.equal(200)` idiom).
    res.getStatus = function () {
        if (typeof __tropel_trp_response_code === 'function') {
            return __tropel_trp_response_code();
        }
        return 0;
    };
    res.getStatusText = function () {
        if (typeof __tropel_trp_response_status === 'function') {
            return __tropel_trp_response_status();
        }
        return '';
    };
    res.getResponseTime = function () {
        if (typeof __tropel_trp_response_time === 'function') {
            return __tropel_trp_response_time();
        }
        return 0;
    };

    // ── Install as non-writable globals ─────────────────────────────────────
    try {
        Object.defineProperty(g, 'bru', { value: bru, writable: false, configurable: false });
        Object.defineProperty(g, 'req', { value: req, writable: false, configurable: false });
        Object.defineProperty(g, 'res', { value: res, writable: false, configurable: false });
    } catch (e) {
        // Tolerate double eval: the bindings are already installed read-only.
    }
})();