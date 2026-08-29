// ── k6-core: the k6 builtins every input format can use ─────────────────────
//
// `check`, `group` and the metric constructors (`Counter`, `Gauge`, `Rate`,
// `Trend`) are k6's API, not Postman's. They lived in `js/scripting-api/pm.js`
// and were installed onto `globalThis` from inside it — so a k6 run, a Bruno
// run and a HAR run all had to load the whole 70 KB Postman shim to get
// `check()`.
//
// That is a layering bug, and it was the thing blocking format-driven shim
// selection (TR-501): "drop pm.js for non-Postman formats" broke `check`, so
// it looked like every format genuinely needed pm. It does not — it needed
// these six symbols.
//
// Split out so the rule can be what it should have been:
//
//   postman  -> k6-core + pm + chai + lodash + cryptojs
//   k6       -> k6-core + k6-shim + deferred-modules
//   bru      -> k6-core + bru        (a peer view over the same __tropel_pm_*
//                                     native bridges, NOT a pm.js dependent)
//   har/openapi/http -> k6-core
//
// State lives in Rust behind the `__tropel_pm_*` bridges, which is why this
// file is a thin redirection layer and why every binding can share it.

// Local copy rather than an import: this shim and pm.js load independently and
// in either order, so a shared helper would create a load-order dependency for
// a five-line predicate.
function isAsyncFunction(fn) {
    if (typeof fn !== 'function') return false;
    var ctor = fn.constructor;
    return !!ctor && (ctor.name === 'AsyncFunction' || ctor.displayName === 'AsyncFunction');
}

// ── group(name, fn) — k6-style grouping ──
// Wraps a block of code in a named group. Emits group_duration
// metric (Trend) showing how long the group took to execute.
// Supports nesting (groups within groups).
// TR-243: k6 rejects ASYNC group callbacks — an async fn returns a
// Promise immediately, so the group would measure ~0ms and its internal
// awaits would run after the group already closed.
function group(name, fn) {
    if (typeof fn === 'function' && isAsyncFunction(fn)) {
        throw new TypeError('group() does not support async callbacks (k6 rejects them)');
    }
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
        // No native group support — run the function directly
        if (typeof fn === 'function') {
            return fn();
        }
    }
}

// ── check(val, conds, tags) — k6-style checks ──
// Evaluates conditions against a value. Each condition is a named
// predicate (function) or a boolean constant (ToBoolean-coerced, k6
// parity — NOT val === condition). Records each as a checks Rate metric
// (pass/fail) with the RAW name as the `check` tag (no "check " prefix,
// k6 parity) plus the optional 3rd-arg tags. Returns true if ALL checks
// pass. Backlog line 149: check(1, null)/check(1, 'x') used to return
// true (nonsense-as-success) — k6 throws for a null/non-object conds;
// and a throwing predicate must record a failed check THEN propagate
// (k6 fails the iteration with that error).
function check(val, conds, tags) {
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
            // TR-243: k6 rejects ASYNC predicates — an async condition
            // returns a Promise, `!!Promise` is truthy, so it would record
            // PASS without ever evaluating. k6 throws instead.
            if (isAsyncFunction(condition)) {
                throw new TypeError(
                    'check() condition "' + name + '" is an async function; k6 rejects async conditions'
                );
            }
            // Predicate function — call with the value. On throw, record
            // the failed check, then let the error propagate (k6 parity).
            try {
                passed = !!condition(val);
            } catch (e) {
                if (typeof __tropel_pm_test === 'function') {
                    __tropel_pm_test(name, false, tagsJson);
                }
                throw e;
            }
        } else {
            // Non-function condition: boolean constant (k6 ToBoolean).
            passed = !!condition;
        }

        // Record the check pass/fail via the existing test bridge — raw
        // name (k6 does not prefix) + optional tags.
        if (typeof __tropel_pm_test === 'function') {
            __tropel_pm_test(name, passed, tagsJson);
        }

        if (!passed) {
            allPassed = false;
        }
    }
    return allPassed;
}

// ── pm.visualizer ──
pm.visualizer = {
    set: function (template, data) {
        // Visualizer is not supported in CLI mode
        console.log('[visualizer] template:', template, 'data:', data);
    }
};

// ── k6-style Custom Metric Constructors ──
// These provide the k6/metrics API: create a metric object, then
// call .add(value, tags) to record a sample with optional tags.
//
// Usage:
//   var counter = new Counter('my_counter');
//   counter.add(1);
//   counter.add(1, { status: '200' });
//
//   var trend = new Trend('my_trend');
//   trend.add(15.5);
//   trend.add(15.5, { status: '200' });

function Counter(name) {
    if (!name || typeof name !== 'string') {
        throw new Error('Counter requires a metric name');
    }
    this._name = name;
    this._type = 'counter';
    this._isTime = false;
}
// TR-243: k6's `.name` is read-only (the metric name is fixed at
// construction). Define an own getter so assignment is a silent no-op in
// sloppy mode and the property always reflects the real name.
Object.defineProperty(Counter.prototype, 'name', {
    configurable: false,
    enumerable: true,
    get: function () { return this._name; },
    set: function () { /* k6: name is read-only */ },
});

Counter.prototype.add = function (value, tags) {
    var v = Number(value);
    // TR-243: k6's `.add()` returns a boolean — true when the value is a
    // finite number (accepted), false when it is NaN/Infinity (silently
    // dropped unless options.throw, which tropel does not implement — the
    // primary-path collector guard drops non-finite anyway).
    if (!isFinite(v)) {
        return false;
    }
    if (typeof __tropel_pm_custom_metric_add === 'function') {
        var tagsStr = tags ? JSON.stringify(tags) : '{}';
        __tropel_pm_custom_metric_add(this._name, v, tagsStr, this._type, this._isTime);
    }
    return true;
};

function Gauge(name) {
    if (!name || typeof name !== 'string') {
        throw new Error('Gauge requires a metric name');
    }
    this._name = name;
    this._type = 'gauge';
    this._isTime = false;
}
Object.defineProperty(Gauge.prototype, 'name', {
    configurable: false,
    enumerable: true,
    get: function () { return this._name; },
    set: function () { /* k6: name is read-only */ },
});

Gauge.prototype.add = function (value, tags) {
    var v = Number(value);
    if (!isFinite(v)) {
        return false;
    }
    if (typeof __tropel_pm_custom_metric_add === 'function') {
        var tagsStr = tags ? JSON.stringify(tags) : '{}';
        __tropel_pm_custom_metric_add(this._name, v, tagsStr, this._type, this._isTime);
    }
    return true;
};

function Rate(name) {
    if (!name || typeof name !== 'string') {
        throw new Error('Rate requires a metric name');
    }
    this._name = name;
    this._type = 'rate';
    this._isTime = false;
}
Object.defineProperty(Rate.prototype, 'name', {
    configurable: false,
    enumerable: true,
    get: function () { return this._name; },
    set: function () { /* k6: name is read-only */ },
});

Rate.prototype.add = function (value, tags) {
    var v = Number(value);
    if (!isFinite(v)) {
        return false;
    }
    if (typeof __tropel_pm_custom_metric_add === 'function') {
        var tagsStr = tags ? JSON.stringify(tags) : '{}';
        __tropel_pm_custom_metric_add(this._name, v, tagsStr, this._type, this._isTime);
    }
    return true;
};

function Trend(name, isTime) {
    if (!name || typeof name !== 'string') {
        throw new Error('Trend requires a metric name');
    }
    this._name = name;
    this._type = 'trend';
    this._isTime = isTime === true;
}
Object.defineProperty(Trend.prototype, 'name', {
    configurable: false,
    enumerable: true,
    get: function () { return this._name; },
    set: function () { /* k6: name is read-only */ },
});

Trend.prototype.add = function (value, tags) {
    var v = Number(value);
    if (!isFinite(v)) {
        return false;
    }
    if (typeof __tropel_pm_custom_metric_add === 'function') {
        var tagsStr = tags ? JSON.stringify(tags) : '{}';
        __tropel_pm_custom_metric_add(this._name, v, tagsStr, this._type, this._isTime);
    }
    return true;
};

    // Expose the k6-style globals the shim also provides (unchanged behavior).
    if (typeof globalThis !== 'undefined') {
        globalThis.postman = postman;
    }
    return pm;
}


// Install the k6 globals. pm.js no longer does this.
if (typeof globalThis !== 'undefined') {
    globalThis.check = check;
    globalThis.group = group;
    globalThis.Counter = Counter;
    globalThis.Gauge = Gauge;
    globalThis.Rate = Rate;
    globalThis.Trend = Trend;
}
