// ── bru.* API for Tropel ───────────────────────────────────────────────────
// Bruno-compat peer view over the shared runtime state.
// Bruno's API shape: `bru.getEnvVar()`, `req.setHeader()`, `res.getBody()`
// (three objects rather than one namespace — P4b). Frozen compat: Bruno.
// Uses the same __tropel_pm_* bridges as the pm binding; the state model
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
        if (typeof __tropel_pm_environment_get === 'function') {
            return __tropel_pm_environment_get(key);
        }
        return null;
    };
    bru.setEnvVar = function (key, value) {
        if (typeof __tropel_pm_environment_set === 'function') {
            __tropel_pm_environment_set(key, String(value));
        }
    };

    // Collection variables (Bruno's bru.getVar/setVar at collection scope)
    bru.getVar = function (key) {
        if (typeof __tropel_pm_collection_vars_get === 'function') {
            var raw = __tropel_pm_collection_vars_get(key);
            if (raw === null || raw === undefined) return undefined;
            try { return JSON.parse(raw); } catch (e) { return raw; }
        }
        return undefined;
    };
    bru.setVar = function (key, value) {
        if (typeof __tropel_pm_collection_vars_set === 'function') {
            __tropel_pm_collection_vars_set(key, value === undefined ? '' : String(value));
        }
    };

    // Request body
    bru.getReqBody = function () {
        if (typeof __tropel_pm_request_body === 'function') {
            return __tropel_pm_request_body();
        }
        return null;
    };
    bru.setReqBody = function (body) {
        if (typeof __tropel_pm_request_body_set === 'function') {
            __tropel_pm_request_body_set(body === undefined ? '' : String(body));
        }
    };

    // Request headers
    bru.getReqHeader = function (name) {
        if (typeof __tropel_pm_request_header_get === 'function') {
            return __tropel_pm_request_header_get(name);
        }
        return null;
    };
    bru.setReqHeader = function (name, value) {
        if (typeof __tropel_pm_request_header_set === 'function') {
            __tropel_pm_request_header_set(name, String(value));
        }
    };

    // Response
    bru.getResBody = function () {
        if (typeof __tropel_pm_response_body === 'function') {
            return __tropel_pm_response_body();
        }
        return null;
    };
    bru.getResHeader = function (name) {
        if (typeof __tropel_pm_response_header === 'function') {
            return __tropel_pm_response_header(name);
        }
        return null;
    };
    bru.getResStatus = function () {
        if (typeof __tropel_pm_response_code === 'function') {
            return __tropel_pm_response_code();
        }
        return 0;
    };
    bru.getResTime = function () {
        if (typeof __tropel_pm_response_time === 'function') {
            return __tropel_pm_response_time();
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
        if (typeof __tropel_pm_test === 'function') {
            __tropel_pm_test(
                'bru.assert: ' + (errorMessage || String(expr)),
                passed ? 1 : 0
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
        if (typeof __tropel_pm_set_next_request === 'function') {
            __tropel_pm_set_next_request(requestName);
        }
    };

    // ── req object (pre-request scripts) ───────────────────────────────────
    var req = {};
    req.setHeader = function (name, value) {
        if (typeof __tropel_pm_request_header_set === 'function') {
            __tropel_pm_request_header_set(name, String(value));
        }
    };
    req.setMethod = function (method) {
        if (typeof __tropel_pm_request_method_set === 'function') {
            __tropel_pm_request_method_set(method);
        }
    };
    req.setUrl = function (url) {
        if (typeof __tropel_pm_request_url_set === 'function') {
            __tropel_pm_request_url_set(url);
        }
    };
    req.setBody = function (body) {
        if (typeof __tropel_pm_request_body_set === 'function') {
            __tropel_pm_request_body_set(body === undefined ? '' : String(body));
        }
    };
    req.getHeader = function (name) {
        if (typeof __tropel_pm_request_header_get === 'function') {
            return __tropel_pm_request_header_get(name);
        }
        return null;
    };
    req.getBody = function () {
        if (typeof __tropel_pm_request_body === 'function') {
            return __tropel_pm_request_body();
        }
        return null;
    };

    // ── res object (test scripts) ──────────────────────────────────────────
    var res = {};
    res.getBody = function () {
        if (typeof __tropel_pm_response_body === 'function') {
            return __tropel_pm_response_body();
        }
        return null;
    };
    res.getHeader = function (name) {
        if (typeof __tropel_pm_response_header === 'function') {
            return __tropel_pm_response_header(name);
        }
        return null;
    };
    // Bruno's res.getStatus() returns the numeric status CODE; res.getStatusText()
    // returns the text (e.g. "OK"). TROPEL_PARITY_BRUNO.md §0: the old
    // implementation returned the text from getStatus() (a silent failure for
    // the canonical `expect(res.getStatus()).to.equal(200)` idiom).
    res.getStatus = function () {
        if (typeof __tropel_pm_response_code === 'function') {
            return __tropel_pm_response_code();
        }
        return 0;
    };
    res.getStatusText = function () {
        if (typeof __tropel_pm_response_status === 'function') {
            return __tropel_pm_response_status();
        }
        return '';
    };
    res.getResponseTime = function () {
        if (typeof __tropel_pm_response_time === 'function') {
            return __tropel_pm_response_time();
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