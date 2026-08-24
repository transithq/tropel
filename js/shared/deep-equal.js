// ══════════════════════════════════════════════════════════════════
// W2 line 190 (TROPEL_MASTER_TODO.md): THE canonical deep-equal.
//
// pm.js `deepEqual`, chai-shim `jsDeepEqual`/`nativeDeepEqual`, and
// lodash-shim `isEqualDeep` used to carry three near-identical copies —
// they agreed on 9/9 inputs today, which is exactly why a fix to one would
// silently skew the others. All three now DELEGATE to
// `globalThis.__tropelDeepEqual` defined here.
//
// Load order: this file must be evaluated FIRST in every bundle (the
// engine's JS_SHIM_BUNDLE / ShimBundle::default, the k6 driver's
// K6_BASE_SHIM_BUNDLE, the web SHIM_SOURCES, @tropel/shims). It is
// IDEMPOTENT — re-evaluation is a no-op, so a test that evals it before
// several shims in sequence is safe.
//
// Semantics (locked by the driver's deep-equal regression tests):
//   • NaN == NaN, key order irrelevant, cycles never overflow the stack
//     (seen-pair guard: revisiting the exact (a,b) pair is assumed equal)
//   • Date by epoch time (two invalid dates compare equal)
//   • RegExp by canonical toString (flag order normalized: /gi == /ig)
//   • Map/Set by size + order-insensitive deep-equal mates
//   • plain objects by sorted key-set + recursive values
// ══════════════════════════════════════════════════════════════════
if (typeof globalThis.__tropelDeepEqual !== 'function') {
    globalThis.__tropelDeepEqual = function __tropelDeepEqual(a, b, seen) {
        if (a === b) return true;
        if (typeof a === 'number' && typeof b === 'number' && isNaN(a) && isNaN(b)) return true;
        if (a === null || b === null || a === undefined || b === undefined) return a === b;
        if (typeof a !== typeof b) return false;
        // Date: compare by epoch time; two invalid dates compare equal.
        // TR-013: guard that BOTH are Date before calling getTime() —
        // the old code threw TypeError when a was non-Date and b was Date.
        if (a instanceof Date || b instanceof Date) {
            if (!(a instanceof Date && b instanceof Date)) return false;
            var ta = a.getTime(), tb = b.getTime();
            return (isNaN(ta) && isNaN(tb)) || ta === tb;
        }
        // RegExp: canonical toString (normalizes flag order — /gi vs /ig are
        // the same expression, matching chai's deep-eql).
        if (a instanceof RegExp || b instanceof RegExp) {
            if (!(b instanceof RegExp)) return false;
            return String(a) === String(b);
        }
        if (Array.isArray(a)) {
            if (!Array.isArray(b) || a.length !== b.length) return false;
            seen = seen || [];
            for (var s = 0; s < seen.length; s++) {
                if (seen[s][0] === a && seen[s][1] === b) return true;
            }
            seen.push([a, b]);
            for (var i = 0; i < a.length; i++) {
                if (!__tropelDeepEqual(a[i], b[i], seen)) {
                    seen.pop();
                    return false;
                }
            }
            seen.pop();
            return true;
        }
        if (typeof a === 'object') {
            if (Array.isArray(b) || b === null || b === undefined) return false;
            // Map: same size, each entry's key+value finds a deep-equal mate
            // (order-insensitive).
            if (a instanceof Map || b instanceof Map) {
                if (!(b instanceof Map) || a.size !== b.size) return false;
                // Cycle guard: Map nodes can be self-referential (a Map whose
                // value is itself); revisiting the exact (a, b) pair mid-compare
                // is assumed equal.
                seen = seen || [];
                for (var s = 0; s < seen.length; s++) {
                    if (seen[s][0] === a && seen[s][1] === b) return true;
                }
                seen.push([a, b]);
                var aEntries = Array.from(a.entries());
                var bEntries = Array.from(b.entries());
                var usedB = [];
                outer:
                for (var mi = 0; mi < aEntries.length; mi++) {
                    for (var mj = 0; mj < bEntries.length; mj++) {
                        if (usedB[mj]) continue;
                        if (__tropelDeepEqual(aEntries[mi][0], bEntries[mj][0], seen) &&
                            __tropelDeepEqual(aEntries[mi][1], bEntries[mj][1], seen)) {
                            usedB[mj] = true;
                            continue outer;
                        }
                    }
                    seen.pop();
                    return false;
                }
                seen.pop();
                return true;
            }
            // Set: same size, each member finds a deep-equal mate.
            if (a instanceof Set || b instanceof Set) {
                if (!(b instanceof Set) || a.size !== b.size) return false;
                // Cycle guard: Set nodes can be self-referential (a Set containing
                // itself); revisiting the exact (a, b) pair mid-compare is assumed
                // equal.
                seen = seen || [];
                for (var s = 0; s < seen.length; s++) {
                    if (seen[s][0] === a && seen[s][1] === b) return true;
                }
                seen.push([a, b]);
                var aMembers = Array.from(a);
                var bMembers = Array.from(b);
                var usedS = [];
                outer2:
                for (var si = 0; si < aMembers.length; si++) {
                    for (var sj = 0; sj < bMembers.length; sj++) {
                        if (usedS[sj]) continue;
                        if (__tropelDeepEqual(aMembers[si], bMembers[sj], seen)) {
                            usedS[sj] = true;
                            continue outer2;
                        }
                    }
                    seen.pop();
                    return false;
                }
                seen.pop();
                return true;
            }
            // Plain object: key-set comparison with a cycle guard.
            seen = seen || [];
            for (var k = 0; k < seen.length; k++) {
                if (seen[k][0] === a && seen[k][1] === b) return true;
            }
            seen.push([a, b]);
            var keysA = Object.keys(a).sort();
            var keysB = Object.keys(b).sort();
            if (keysA.length !== keysB.length) {
                seen.pop();
                return false;
            }
            for (var j = 0; j < keysA.length; j++) {
                if (keysA[j] !== keysB[j] || !__tropelDeepEqual(a[keysA[j]], b[keysB[j]], seen)) {
                    seen.pop();
                    return false;
                }
            }
            seen.pop();
            return true;
        }
        return a === b;
    };
}
