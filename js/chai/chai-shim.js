// ─── Chai Assertion Library Shim for Tropel ──────────────
// A simplified chai-compatible assertion library that delegates
// heavy operations to the native Rust assert module.

// Global chai
var chai = chai || {};

(function () {
    // W2 line 190: single canonical deep-equal — js/shared/deep-equal.js
    // (globalThis.__tropelDeepEqual), evaluated FIRST in every bundle. The
    // per-shim copy is gone and so is the dead __tropel_native_deep_equal
    // bridge (registered nowhere): chai captured it at LOAD time while
    // lodash checked at CALL time, so if it were ever registered the two
    // shims would have DIVERGED. Both names now delegate to the canonical.
    function jsDeepEqual(a, b) {
        return globalThis.__tropelDeepEqual(a, b);
    }
    var nativeDeepEqual = jsDeepEqual;

    // chai's type() name for a value (backlog line 104: a/an were not
    // callable — they were plain getters returning `this`, so
    // `expect(x).to.be.a('string')` threw "a is not a function").
    function chaiTypeName(obj) {
        if (obj === null) return 'null';
        if (Array.isArray(obj)) return 'array';
        var t = typeof obj;
        if (t === 'object') {
            if (obj instanceof Date) return 'date';
            if (obj instanceof RegExp) return 'regexp';
            if (obj instanceof Error) return 'error';
            if (typeof Map !== 'undefined' && obj instanceof Map) return 'map';
            if (typeof Set !== 'undefined' && obj instanceof Set) return 'set';
            if (typeof WeakMap !== 'undefined' && obj instanceof WeakMap) return 'weakmap';
            if (typeof WeakSet !== 'undefined' && obj instanceof WeakSet) return 'weakset';
            if (typeof Promise !== 'undefined' && obj instanceof Promise) return 'promise';
            return 'object';
        }
        return t; // string, number, boolean, function, symbol, bigint, undefined
    }

    // Shared type check for a/an. `type` may be a chai type name ('string',
    // 'array', 'object', 'null', ...) or a constructor function (instanceof).
    function assertTypeMatches(obj, type) {
        if (typeof type === 'function') {
            return obj instanceof type;
        }
        return chaiTypeName(obj) === type;
    }

    // ── Assertion Constructor ──
    function Assertion(obj, msg, ssfi) {
        this._obj = obj;
        this._msg = msg;
        this._ssfi = ssfi || Assertion;
        // Initialize the flags bag up front. Without this, a plain chain
        // (no `.not`/`.deep` accessed) leaves `__flags` undefined and the
        // `var negate = this.__flags && this.__flags.negate` idiom below
        // yields `undefined` — and `(false) !== undefined` is TRUE, so every
        // positive assertion silently passed. The `!!(...)` conversions below
        // are the actual fix; this init keeps the getters' `|| {}` harmless.
        this.__flags = {};
    }

    // ── Chainable properties ──
    Object.defineProperties(Assertion.prototype, {
        to: { get: function () { return this; }, enumerable: true },
        be: { get: function () { return this; }, enumerable: true },
        been: { get: function () { return this; }, enumerable: true },
        is: { get: function () { return this; }, enumerable: true },
        that: { get: function () { return this; }, enumerable: true },
        which: { get: function () { return this; }, enumerable: true },
        and: { get: function () { return this; }, enumerable: true },
        has: { get: function () { return this; }, enumerable: true },
        have: { get: function () { return this; }, enumerable: true },
        with: { get: function () { return this; }, enumerable: true },
        at: { get: function () { return this; }, enumerable: true },
        of: { get: function () { return this; }, enumerable: true },
        same: { get: function () { return this; }, enumerable: true },
        not: {
            get: function () {
                this.__flags = this.__flags || {};
                this.__flags.negate = true;
                return this;
            },
            enumerable: true
        },
        deep: {
            get: function () {
                this.__flags = this.__flags || {};
                this.__flags.deep = true;
                return this;
            },
            enumerable: true
        },
        // Backlog line 104: a/an are CHAINABLE METHODS in chai — calling
        // them asserts the type (`expect('x').to.be.a('string')`) and returns
        // the assertion for chaining; the no-argument form stays a no-op
        // chain getter. The Proxy (chai.expect) returns a variant that hands
        // back the proxy so the unknown-name guard stays active afterwards.
        a: {
            get: function () {
                var self = this;
                return function (type) {
                    if (type !== undefined) {
                        var negate = !!(self.__flags && self.__flags.negate);
                        var passed = assertTypeMatches(self._obj, type) !== negate;
                        if (!passed) {
                            throw new Error(
                                (self._msg ? self._msg + ': ' : '') +
                                'expected ' + JSON.stringify(self._obj) +
                                (negate ? ' not' : '') + ' to be a ' + (typeof type === 'function' ? type.name || 'constructor' : String(type))
                            );
                        }
                    }
                    return self;
                };
            },
            enumerable: true
        },
        an: {
            get: function () {
                var self = this;
                return function (type) {
                    if (type !== undefined) {
                        var negate = !!(self.__flags && self.__flags.negate);
                        var passed = assertTypeMatches(self._obj, type) !== negate;
                        if (!passed) {
                            throw new Error(
                                (self._msg ? self._msg + ': ' : '') +
                                'expected ' + JSON.stringify(self._obj) +
                                (negate ? ' not' : '') + ' to be an ' + (typeof type === 'function' ? type.name || 'constructor' : String(type))
                            );
                        }
                    }
                    return self;
                };
            },
            enumerable: true
        }
    });

    // ── Assertion Methods ──

    // .equal(expected)
    // Real chai: `.deep.equal(x)` performs a DEEP comparison (the `deep`
    // chainable getter sets `__flags.deep`, and `equal` must honor it —
    // W1-A exposed that `.deep.equal` silently did a shallow `===`, so
    // `expect({a:1}).to.deep.equal({a:1})` threw).
    Assertion.prototype.equal = function (value) {
        var negate = !!(this.__flags && this.__flags.negate);
        var deep = !!(this.__flags && this.__flags.deep);
        var passed = (deep ? nativeDeepEqual(this._obj, value) : this._obj === value) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + (deep ? ' to deeply equal ' : ' to equal ') + JSON.stringify(value)
            );
        }
        return this;
    };

    // .eql(expected) — deep equality (chai's .eql is ALWAYS deep, so the
    // deep flag is irrelevant here; the `if (deep || true)` dead branch is
    // gone — line 146 of the master TODO).
    Assertion.prototype.eql = function (value) {
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = nativeDeepEqual(this._obj, value) !== negate;

        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + ' to deeply equal ' + JSON.stringify(value)
            );
        }
        return this;
    };

    // .include(value) — deep-aware (W1-B: `.deep.include({name:"x"})` must
    // deep-include; previously the deep flag was ignored and only the
    // shallow `value in obj` / indexOf paths existed).
    Assertion.prototype.include = function (value) {
        var obj = this._obj;
        var negate = !!(this.__flags && this.__flags.negate);
        var deep = !!(this.__flags && this.__flags.deep);
        var passed;

        if (typeof obj === 'string') {
            passed = (obj.indexOf(value) !== -1) !== negate;
        } else if (Array.isArray(obj)) {
            if (deep) {
                var found = false;
                for (var i = 0; i < obj.length; i++) {
                    if (nativeDeepEqual(obj[i], value)) { found = true; break; }
                }
                passed = found !== negate;
            } else {
                passed = (obj.indexOf(value) !== -1) !== negate;
            }
        } else if (typeof obj === 'object' && obj !== null) {
            if (deep) {
                // chai subset semantics: every key of `value` must exist on
                // `obj` with a deep-equal value (extra keys on obj allowed).
                // Only valid when `value` is itself an object — a primitive
                // has no keys, so Object.keys(5) = [] would vacuously pass
                // (false-green; Object.keys(null) would throw). Fail instead.
                if (value === null || typeof value !== 'object') {
                    passed = false !== negate;
                } else {
                    var expKeys = Object.keys(value);
                    var allMatch = expKeys.every(function (k) {
                        return k in obj && nativeDeepEqual(obj[k], value[k]);
                    });
                    passed = allMatch !== negate;
                }
            } else {
                passed = (value in obj) !== negate;
            }
        } else {
            passed = false;
        }

        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(obj) +
                (negate ? ' not' : '') + (deep ? ' to deeply include ' : ' to include ') + JSON.stringify(value)
            );
        }
        return this;
    };

    // .ok
    Object.defineProperty(Assertion.prototype, 'ok', {
        get: function () {
            var negate = !!(this.__flags && this.__flags.negate);
            var passed = !!this._obj !== negate;
            if (!passed) {
                throw new Error(
                    (this._msg ? this._msg + ': ' : '') +
                    'expected ' + JSON.stringify(this._obj) +
                    (negate ? ' not' : '') + ' to be truthy'
                );
            }
            return this;
        },
        enumerable: true
    });

    // .true
    Object.defineProperty(Assertion.prototype, 'true', {
        get: function () {
            var negate = !!(this.__flags && this.__flags.negate);
            var passed = (this._obj === true) !== negate;
            if (!passed) {
                throw new Error(
                    'expected ' + this._obj +
                    (negate ? ' not' : '') + ' to be true'
                );
            }
            return this;
        },
        enumerable: true
    });

    // .false
    Object.defineProperty(Assertion.prototype, 'false', {
        get: function () {
            var negate = !!(this.__flags && this.__flags.negate);
            var passed = (this._obj === false) !== negate;
            if (!passed) {
                throw new Error(
                    'expected ' + this._obj +
                    (negate ? ' not' : '') + ' to be false'
                );
            }
            return this;
        },
        enumerable: true
    });

    // .null
    Object.defineProperty(Assertion.prototype, 'null', {
        get: function () {
            var negate = !!(this.__flags && this.__flags.negate);
            var passed = (this._obj === null) !== negate;
            if (!passed) {
                throw new Error(
                    'expected ' + JSON.stringify(this._obj) +
                    (negate ? ' not' : '') + ' to be null'
                );
            }
            return this;
        },
        enumerable: true
    });

    // .undefined
    Object.defineProperty(Assertion.prototype, 'undefined', {
        get: function () {
            var negate = !!(this.__flags && this.__flags.negate);
            var passed = (this._obj === undefined) !== negate;
            if (!passed) {
                throw new Error(
                    'expected ' + JSON.stringify(this._obj) +
                    (negate ? ' not' : '') + ' to be undefined'
                );
            }
            return this;
        },
        enumerable: true
    });

    // .property(name[, value])
    Assertion.prototype.property = function (name, value) {
        var obj = this._obj;
        var negate = !!(this.__flags && this.__flags.negate);
        var has = obj !== null && obj !== undefined && name in obj;
        var passed = has !== negate;

        if (passed && value !== undefined) {
            passed = (obj[name] === value) !== negate;
        }

        if (!passed) {
            throw new Error(
                'expected ' + JSON.stringify(obj) +
                (negate ? ' not' : '') + ' to have property ' + name
            );
        }
        return this;
    };

    // .lengthOf(n)
    Assertion.prototype.lengthOf = function (n) {
        var obj = this._obj;
        var negate = !!(this.__flags && this.__flags.negate);
        var passed;

        if (typeof obj === 'string' || Array.isArray(obj)) {
            passed = (obj.length === n) !== negate;
        } else if (typeof obj === 'object' && obj !== null) {
            passed = (Object.keys(obj).length === n) !== negate;
        } else {
            passed = false;
        }

        if (!passed) {
            throw new Error(
                'expected ' + JSON.stringify(obj) +
                (negate ? ' not' : '') + ' to have length ' + n
            );
        }
        return this;
    };

    // .match(regexp)
    Assertion.prototype.match = function (re) {
        var obj = String(this._obj);
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = re.test(obj) !== negate;
        if (!passed) {
            throw new Error(
                'expected ' + JSON.stringify(obj) +
                (negate ? ' not' : '') + ' to match ' + re
            );
        }
        return this;
    };

    // .string(string)
    Assertion.prototype.string = function (str) {
        var obj = String(this._obj);
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (obj.indexOf(str) !== -1) !== negate;
        if (!passed) {
            throw new Error(
                'expected ' + JSON.stringify(obj) +
                (negate ? ' not' : '') + ' to contain ' + JSON.stringify(str)
            );
        }
        return this;
    };

    // .keys(key1, key2, ...)
    Assertion.prototype.keys = function () {
        var obj = this._obj;
        var expectedKeys = Array.prototype.slice.call(arguments);
        var negate = !!(this.__flags && this.__flags.negate);
        var passed;

        if (expectedKeys.length === 1 && Array.isArray(expectedKeys[0])) {
            expectedKeys = expectedKeys[0];
        }

        if (obj && typeof obj === 'object') {
            var objKeys = Object.keys(obj);
            passed = expectedKeys.every(function (k) { return objKeys.indexOf(k) !== -1; }) !== negate;
        } else {
            passed = false;
        }

        if (!passed) {
            throw new Error(
                'expected ' + JSON.stringify(obj) +
                (negate ? ' not' : '') + ' to have keys ' + JSON.stringify(expectedKeys)
            );
        }
        return this;
    };

    // ── Backlog line 104: missing assertion METHODS ──
    // above/below/least/most/contain/instanceof/oneOf/throw were absent —
    // accessing them hit the Proxy guard and threw "unknown assertion
    // property", turning a large slice of valid chai red. These mirror
    // chai's semantics, each returning `this` for chaining.

    // .above(n) / .gt(n) / .greaterThan(n) — obj > n
    Assertion.prototype.above = function (n) {
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (this._obj > n) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + ' to be above ' + n
            );
        }
        return this;
    };
    Assertion.prototype.gt = Assertion.prototype.above;
    Assertion.prototype.greaterThan = Assertion.prototype.above;

    // .below(n) / .lt(n) / .lessThan(n) — obj < n
    Assertion.prototype.below = function (n) {
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (this._obj < n) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + ' to be below ' + n
            );
        }
        return this;
    };
    Assertion.prototype.lt = Assertion.prototype.below;
    Assertion.prototype.lessThan = Assertion.prototype.below;

    // .least(n) / .gte(n) — obj >= n (chai: `.at.least(n)`, `at` chains)
    Assertion.prototype.least = function (n) {
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (this._obj >= n) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + ' to be at least ' + n
            );
        }
        return this;
    };
    Assertion.prototype.gte = Assertion.prototype.least;

    // .most(n) / .lte(n) — obj <= n (chai: `.at.most(n)`)
    Assertion.prototype.most = function (n) {
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (this._obj <= n) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + ' to be at most ' + n
            );
        }
        return this;
    };
    Assertion.prototype.lte = Assertion.prototype.most;

    // .within(start, finish) — start <= obj <= finish (chai inclusive)
    Assertion.prototype.within = function (start, finish) {
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (this._obj >= start && this._obj <= finish) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + ' to be within ' + start + '..' + finish
            );
        }
        return this;
    };

    // .closeTo(expected, delta) — |obj - expected| <= delta (chai)
    Assertion.prototype.closeTo = function (expected, delta) {
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (Math.abs(this._obj - expected) <= delta) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + ' to be close to ' + expected + ' +/- ' + delta
            );
        }
        return this;
    };

    // .members(list) — chai's plain .members asserts the target array has
    // the SAME members as list, order-insensitive (set equality: same size
    // AND every element of list present in the target). The superset
    // spelling is `.include.members`, which this shim does not split — a
    // plain .members that only checked one direction would let
    // `[1,2,3].members([1,2])` PASS (a false-green, the exact failure mode
    // this project hunts). Deep-aware: with the deep flag, membership is by
    // deep equality.
    Assertion.prototype.members = function (list) {
        var negate = !!(this.__flags && this.__flags.negate);
        var deep = !!(this.__flags && this.__flags.deep);
        var obj = this._obj;
        var passed;
        if (Array.isArray(obj) && Array.isArray(list)) {
            // Multiset semantics (chai counts occurrences): same length AND
            // each list element finds a distinct mate in the target — a naive
            // length + every/some would let [1,1,2].members([1,2,2]) PASS
            // (false-green). Deep flag compares by deep equality.
            var used = [];
            var all = obj.length === list.length && list.every(function (needle) {
                for (var i = 0; i < obj.length; i++) {
                    if (used[i]) continue;
                    if (deep ? nativeDeepEqual(obj[i], needle) : obj[i] === needle) {
                        used[i] = true;
                        return true;
                    }
                }
                return false;
            });
            passed = all !== negate;
        } else {
            passed = false;
        }
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(obj) +
                (negate ? ' not' : '') + (deep ? ' to deeply ' : ' to ') + 'have members ' + JSON.stringify(list)
            );
        }
        return this;
    };

    // .contain(value) — alias of .include (chai exposes both)
    Assertion.prototype.contain = function (value) {
        return this.include(value);
    };
    Assertion.prototype.contains = Assertion.prototype.contain;

    // .instanceof(Ctor) / .instanceOf(Ctor) — instanceof check
    Assertion.prototype.instanceof = function (Ctor) {
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (this._obj instanceof Ctor) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + ' to be an instance of ' + (Ctor && Ctor.name ? Ctor.name : String(Ctor))
            );
        }
        return this;
    };
    Assertion.prototype.instanceOf = Assertion.prototype.instanceof;

    // .oneOf(list) — obj is a member of the list (deep-compare each element)
    Assertion.prototype.oneOf = function (list) {
        var negate = !!(this.__flags && this.__flags.negate);
        var found = false;
        if (Array.isArray(list)) {
            for (var i = 0; i < list.length; i++) {
                if (nativeDeepEqual(this._obj, list[i])) {
                    found = true;
                    break;
                }
            }
        }
        var passed = found !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) +
                (negate ? ' not' : '') + ' to be one of ' + JSON.stringify(list)
            );
        }
        return this;
    };

    // .throw([errorCtorOrMsg][, msgMatch]) — the target must be a function
    // that throws; optionally checks the thrown error's constructor and/or
    // message. Mirrors chai's throw/throws (error constructor OR string /
    // regexp message match).
    Assertion.prototype.throw = function (errType, errMsg) {
        var fn = this._obj;
        if (typeof fn !== 'function') {
            throw new Error('expected ' + JSON.stringify(this._obj) + ' to be a function');
        }
        var negate = !!(this.__flags && this.__flags.negate);
        var threw = false;
        var caught = null;
        try {
            fn();
        } catch (e) {
            threw = true;
            caught = e;
        }
        var passed = threw;
        if (passed && errType !== undefined && errType !== null) {
            if (typeof errType === 'function') {
                passed = caught instanceof errType;
            } else if (errType instanceof RegExp) {
                passed = errType.test(String(caught && caught.message));
            } else {
                passed = caught && caught.message === String(errType);
            }
        }
        if (passed && errMsg !== undefined) {
            if (errMsg instanceof RegExp) {
                passed = errMsg.test(String(caught && caught.message));
            } else {
                passed = caught && caught.message === String(errMsg);
            }
        }
        passed = passed !== negate;
        if (!passed) {
            var want = 'to throw';
            if (errType !== undefined && errType !== null) {
                want += ' ' + (typeof errType === 'function' ? errType.name : String(errType));
            }
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected ' + JSON.stringify(this._obj) + (negate ? ' not' : '') + ' ' + want
            );
        }
        return this;
    };
    Assertion.prototype.throws = Assertion.prototype.throw;

    // ── guarded assertion Proxy ──
    // Backlog §1/§2: unimplemented assertion PROPERTIES (e.g. `.to.be.empty`,
    // `.to.exist`, `.NaN`, `.finite`, `.sealed`) used to read as `undefined`
    // and the enclosing pm.test/check recorded GREEN — a silent pass. The
    // instance is wrapped in a Proxy whose `get` trap THROWS on unknown
    // assertion names, so a typo'd or unimplemented assertion fails instead
    // of passing silently. Known names resolve normally through the prototype
    // chain. Shared by chai.expect and the `.should` getter (backlog §2: the
    // latter used to return a RAW Assertion, leaving the whole silent-pass
    // class reachable via `.should`).
    function guardAssertion(assertion) {
        return new Proxy(assertion, {
            get: function (target, prop, receiver) {
                // Symbols (Symbol.toPrimitive etc.) and the standard
                // inspection/promise interop names must resolve normally — a
                // `console.log(assertion)` or `JSON.stringify(assertion)`
                // would otherwise hit the guard and throw spuriously.
                if (
                    typeof prop === 'symbol' ||
                    prop === 'then' || prop === 'toJSON' || prop === 'inspect'
                ) {
                    return Reflect.get(target, prop, receiver);
                }
                if (prop === 'a' || prop === 'an') {
                    // Chainable-method variant: the returned function hands
                    // back THIS proxy so `expect(x).to.be.a('string').bogus`
                    // still trips the unknown-name guard instead of silently
                    // returning a raw assertion.
                    var raw = Reflect.get(target, prop, receiver);
                    return function (type) {
                        raw.call(target, type);
                        return receiver;
                    };
                }
                // W1-A: `prop in …` leaked Object.prototype — `.toString`,
                // `.constructor`, `.hasOwnProperty`, `.__proto__`, `._obj`,
                // `.__flags` all resolved (and recorded PASS) instead of
                // throwing, so a typo could never fail. Own-property checks
                // only: real assertions are defineProperty'd onto the
                // Assertion prototype, and instance state (_obj/_flags) lives
                // on the instance itself. `constructor` is an OWN property of
                // every prototype object (the standard back-reference), so it
                // would slip through the own-property check — reject it
                // explicitly; it is not an assertion member.
                if (
                    prop !== 'constructor' &&
                    (Object.prototype.hasOwnProperty.call(Assertion.prototype, prop) ||
                        Object.prototype.hasOwnProperty.call(target, prop))
                ) {
                    return Reflect.get(target, prop, receiver);
                }
                throw new Error("unknown assertion property '" + String(prop) + "'");
            }
        });
    }

    // ── chai.expect ──
    chai.expect = function (val, msg) {
        return guardAssertion(new Assertion(val, msg, chai.expect));
    };

    // ── Postman extensions (chai-postman parity) ──
    // pm.expect delegates to chai's Assertion (W1-B), so the
    // Postman-specific members status/header/jsonBody must live here too.
    // They read pm.response, which exists at call time whenever pm.js is
    // loaded (the runtime bundle always loads both; chai alone leaves them
    // failing on an absent pm.response, which is correct — these are
    // Postman members, not core chai).
    Assertion.prototype.status = function (code) {
        var actual = (typeof pm !== 'undefined' && pm.response) ? pm.response.code : undefined;
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (actual === code) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected response ' + (negate ? 'not ' : '') + 'to have status ' + code + ' but got ' + actual
            );
        }
        return this;
    };
    Assertion.prototype.header = function (key, value) {
        var header = (typeof pm !== 'undefined' && pm.response) ? pm.response.header(key) : undefined;
        var negate = !!(this.__flags && this.__flags.negate);
        var passed = (header === value) !== negate;
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected header ' + key + ' ' + (negate ? 'not ' : '') +
                'to be ' + JSON.stringify(value) + ', got ' + JSON.stringify(header)
            );
        }
        return this;
    };
    Assertion.prototype.jsonBody = function (expected, expectedValue) {
        var body = (typeof pm !== 'undefined' && pm.response) ? pm.response.json() : undefined;
        var negate = !!(this.__flags && this.__flags.negate);
        var passed;
        if (typeof expected === 'string') {
            // W1-B line 153: chai-postman treats a STRING as a KEY PATH, not
            // a deep-equal of the whole body — the old code deep-equal'd
            // `body` against the string, so `to.have.jsonBody('key')` always
            // threw on an object body (false failure). `jsonBody('a.b')`
            // asserts the path EXISTS; `jsonBody('a.b', 7)` asserts the
            // value at that path.
            var parts = expected.split('.');
            var node = body;
            // lodash `get` parity: only `undefined` at the FINAL segment
            // means MISSING (a present-null key like `{a: null}` passes
            // `jsonBody('a')`), but a null MID-path stops the walk — so
            // track `reached` to tell "final value is null" from "stopped
            // mid-path" (the latter is a missing path, so a negated
            // `.not.jsonBody('a.b')` on `{a:null}` must PASS).
            var reached = 0;
            for (; reached < parts.length && node !== undefined && node !== null; reached++) {
                node = node[parts[reached]];
            }
            var hasKey = reached === parts.length && node !== undefined;
            if (expectedValue !== undefined) {
                passed = (hasKey && nativeDeepEqual(node, expectedValue)) !== negate;
            } else {
                passed = hasKey !== negate;
            }
        } else {
            passed = nativeDeepEqual(body, expected) !== negate;
        }
        if (!passed) {
            throw new Error(
                (this._msg ? this._msg + ': ' : '') +
                'expected response body ' + (negate ? 'not ' : '') + 'to match'
            );
        }
        return this;
    };

    // .empty
    Object.defineProperty(Assertion.prototype, 'empty', {
        get: function () {
            var negate = !!(this.__flags && this.__flags.negate);
            var obj = this._obj;
            var empty = (typeof obj === 'string' || Array.isArray(obj))
                ? obj.length === 0
                : (obj !== null && typeof obj === 'object') && Object.keys(obj).length === 0;
            var passed = empty !== negate;
            if (!passed) {
                throw new Error(
                    (this._msg ? this._msg + ': ' : '') +
                    'expected ' + JSON.stringify(obj) + (negate ? ' not' : '') + ' to be empty'
                );
            }
            return this;
        },
        enumerable: true
    });

    // .exist
    Object.defineProperty(Assertion.prototype, 'exist', {
        get: function () {
            var negate = !!(this.__flags && this.__flags.negate);
            var exists = this._obj !== null && this._obj !== undefined;
            var passed = exists !== negate;
            if (!passed) {
                throw new Error(
                    (this._msg ? this._msg + ': ' : '') +
                    'expected ' + JSON.stringify(this._obj) + (negate ? ' not' : '') + ' to exist'
                );
            }
            return this;
        },
        enumerable: true
    });

    // .NaN
    Object.defineProperty(Assertion.prototype, 'NaN', {
        get: function () {
            var negate = !!(this.__flags && this.__flags.negate);
            var nan = typeof this._obj === 'number' && isNaN(this._obj);
            var passed = nan !== negate;
            if (!passed) {
                throw new Error(
                    (this._msg ? this._msg + ': ' : '') +
                    'expected ' + JSON.stringify(this._obj) + (negate ? ' not' : '') + ' to be NaN'
                );
            }
            return this;
        },
        enumerable: true
    });

    // .finite
    Object.defineProperty(Assertion.prototype, 'finite', {
        get: function () {
            var negate = !!(this.__flags && this.__flags.negate);
            var finite = typeof this._obj === 'number' && isFinite(this._obj);
            var passed = finite !== negate;
            if (!passed) {
                throw new Error(
                    (this._msg ? this._msg + ': ' : '') +
                    'expected ' + JSON.stringify(this._obj) + (negate ? ' not' : '') + ' to be finite'
                );
            }
            return this;
        },
        enumerable: true
    });

    // ── chai.assert ──
    chai.assert = {
        isOk: function (val, msg) {
            if (!val) throw new Error(msg || 'expected ' + JSON.stringify(val) + ' to be truthy');
        },
        isNotOk: function (val, msg) {
            if (val) throw new Error(msg || 'expected ' + JSON.stringify(val) + ' to be falsy');
        },
        equal: function (act, exp, msg) {
            if (act !== exp) throw new Error(msg || 'expected ' + JSON.stringify(act) + ' to equal ' + JSON.stringify(exp));
        },
        notEqual: function (act, exp, msg) {
            if (act === exp) throw new Error(msg || 'expected ' + JSON.stringify(act) + ' not to equal ' + JSON.stringify(exp));
        },
        deepEqual: function (act, exp, msg) {
            if (!nativeDeepEqual(act, exp)) throw new Error(msg || 'expected deep equality');
        },
        isTrue: function (val, msg) {
            if (val !== true) throw new Error(msg || 'expected ' + JSON.stringify(val) + ' to be true');
        },
        isFalse: function (val, msg) {
            if (val !== false) throw new Error(msg || 'expected ' + JSON.stringify(val) + ' to be false');
        },
        isNull: function (val, msg) {
            if (val !== null) throw new Error(msg || 'expected null');
        },
        isNotNull: function (val, msg) {
            if (val === null) throw new Error(msg || 'expected not null');
        },
        isUndefined: function (val, msg) {
            if (val !== undefined) throw new Error(msg || 'expected undefined');
        },
        isDefined: function (val, msg) {
            if (val === undefined) throw new Error(msg || 'expected defined');
        },
        isString: function (val, msg) {
            if (typeof val !== 'string') throw new Error(msg || 'expected a string');
        },
        isNumber: function (val, msg) {
            if (typeof val !== 'number') throw new Error(msg || 'expected a number');
        },
        isBoolean: function (val, msg) {
            if (typeof val !== 'boolean') throw new Error(msg || 'expected a boolean');
        },
        isArray: function (val, msg) {
            if (!Array.isArray(val)) throw new Error(msg || 'expected an array');
        },
        isObject: function (val, msg) {
            if (typeof val !== 'object' || val === null || Array.isArray(val)) throw new Error(msg || 'expected an object');
        },
        isFunction: function (val, msg) {
            if (typeof val !== 'function') throw new Error(msg || 'expected a function');
        },
        include: function (haystack, needle, msg) {
            if (haystack.indexOf(needle) === -1) throw new Error(msg || 'expected to include ' + JSON.stringify(needle));
        },
        match: function (val, re, msg) {
            if (!re.test(val)) throw new Error(msg || 'expected to match ' + re);
        },
        lengthOf: function (val, n, msg) {
            if (val.length !== n) throw new Error(msg || 'expected length ' + n + ' got ' + val.length);
        },
        fail: function (msg) {
            throw new Error(msg || 'Assertion failed');
        },
        throws: function (fn, err, msg) {
            // The "expected function to throw" error must be thrown AFTER the
            // try/catch — previously it was thrown inside the try and then
            // caught by its own catch, so assert.throws() always passed even
            // when nothing threw (the error fell through and returned).
            var threw = false;
            var caught;
            try {
                fn();
            } catch (e) {
                threw = true;
                caught = e;
            }
            if (!threw) {
                throw new Error(msg || 'expected function to throw');
            }
            if (err && typeof err === 'function' && !(caught instanceof err)) {
                throw new Error(
                    msg || 'expected function to throw ' + err.name + ' but threw ' + (caught && caught.name)
                );
            }
            if (typeof err === 'string' && caught.message !== err) {
                throw new Error(msg || 'expected error message ' + err + ' got ' + caught.message);
            }
        },
        doesNotThrow: function (fn, msg) {
            try {
                fn();
            } catch (e) {
                throw new Error(msg || 'expected function not to throw: ' + e.message);
            }
        }
    };

    // ── chai.should (minimal) ──
    // The getter returns the SAME guarded Proxy as chai.expect (backlog §2:
    // it used to return a raw Assertion, so `({a:1}).should.be.sealed` read
    // `undefined` and recorded GREEN).
    chai.should = function () {
        Object.defineProperty(Object.prototype, 'should', {
            get: function () {
                // `(5).should` boxes the primitive — `this` is `new Number(5)`
                // and `new Number(5) === 5` is false, so `(5).should.equal(5)`
                // would throw "expected 5 to equal 5" with the boxed _obj.
                // Unbox Number/String/Boolean wrappers (W1-A regression test
                // exposed this; Date etc. keep their valueOf untouched).
                var obj = this;
                if (
                    typeof obj === 'object' && obj !== null &&
                    (obj instanceof Number || obj instanceof String || obj instanceof Boolean)
                ) {
                    obj = obj.valueOf();
                }
                return guardAssertion(new Assertion(obj));
            },
            set: function () {},
            configurable: true,
            enumerable: false
        });
    };
})();

// Export for module systems
if (typeof module !== 'undefined' && module.exports) {
    module.exports = chai;
}
