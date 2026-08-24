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
        // RegExp: compare only RegExps, then use canonical toString (which
        // normalizes flag order — /gi vs /ig are the same expression).
        if (a instanceof RegExp || b instanceof RegExp) {
            if (!(a instanceof RegExp && b instanceof RegExp)) return false;
            return String(a) === String(b);
        }
        // ArrayBuffer: compare byte-by-byte.
        if (a instanceof ArrayBuffer || b instanceof ArrayBuffer) {
            if (!(a instanceof ArrayBuffer) || !(b instanceof ArrayBuffer)) return false;
            if (a.byteLength !== b.byteLength) return false;
            var va = new Uint8Array(a), vb = new Uint8Array(b);
            for (var i = 0; i < va.length; i++) {
                if (va[i] !== vb[i]) return false;
            }
            return true;
        }
        // DataView: it is an ArrayBuffer view but has no indexed elements.
        // Compare the viewed bytes, not the backing buffer's unrelated bytes.
        if (a instanceof DataView || b instanceof DataView) {
            if (!(a instanceof DataView && b instanceof DataView)) return false;
            if (a.byteLength !== b.byteLength) return false;
            var da = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
            var db = new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
            for (var i = 0; i < da.length; i++) {
                if (da[i] !== db[i]) return false;
            }
            return true;
        }
        // TypedArray: same constructor, same byte length, byte-by-byte.
        if (ArrayBuffer.isView(a) && !(a instanceof DataView) ||
            ArrayBuffer.isView(b) && !(b instanceof DataView)) {
            if (!ArrayBuffer.isView(a) || !ArrayBuffer.isView(b)) return false;
            if (a.constructor !== b.constructor) return false;
            if (a.byteLength !== b.byteLength) return false;
            var ta = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
            var tb = new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
            for (var i = 0; i < ta.length; i++) {
                if (ta[i] !== tb[i]) return false;
            }
            return true;
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
            // Error: compare its identifying fields and enumerable own fields.
            // The latter covers custom metadata and nested/circular causes while
            // retaining the existing constructor/message/name semantics.
            if (a instanceof Error || b instanceof Error) {
                if (!(a instanceof Error) || !(b instanceof Error)) return false;
                if (a.constructor !== b.constructor || a.message !== b.message || a.name !== b.name) {
                    return false;
                }
                seen = seen || [];
                for (var e = 0; e < seen.length; e++) {
                    if (seen[e][0] === a && seen[e][1] === b) return true;
                }
                seen.push([a, b]);
                var errorKeysA = Object.keys(a).sort();
                var errorKeysB = Object.keys(b).sort();
                if (errorKeysA.length !== errorKeysB.length) {
                    seen.pop();
                    return false;
                }
                for (var ei = 0; ei < errorKeysA.length; ei++) {
                    if (errorKeysA[ei] !== errorKeysB[ei] ||
                        !__tropelDeepEqual(a[errorKeysA[ei]], b[errorKeysB[ei]], seen)) {
                        seen.pop();
                        return false;
                    }
                }
                seen.pop();
                return true;
            }
            // Non-plain objects (Promise, WeakMap, WeakSet, and other host
            // objects):
            // reference equality only. Two distinct Promise objects are never
            // deep-equal, even if they resolve to the same value.
            if (Object.getPrototypeOf(a) !== Object.prototype ||
                Object.getPrototypeOf(b) !== Object.prototype) {
                return a === b;
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
