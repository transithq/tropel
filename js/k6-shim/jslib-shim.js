// ══════════════════════════════════════════════════════════════════
// k6 jslib Shim — provides the https://jslib.k6.io/ module APIs as
// globals (backlog line 118: jslib URL imports are stripped by the
// transpiler because they cannot be fetched locally, but until now the
// APIs they imported had ZERO definitions — so `randomIntBetween`,
// `uuidv4`, `htmlReport` etc. threw ReferenceError in the default fn
// and silently degraded to the default summary in handleSummary).
//
// These mirror the k6-utils (randomIntBetween / randomItem /
// randomString / uuidv4 / findBetween) and k6-summary (textSummary /
// htmlReport) modules from jslib.k6.io. All randomness flows through
// `Math.random`, so k6's `randomSeed()` (which replaces Math.random
// with a mulberry32 PRNG) makes every jslib value deterministic per
// VU, exactly k6's contract.
// ══════════════════════════════════════════════════════════════════

// ── k6-utils ──────────────────────────────────────────────────────

// Random integer in [min, max] (inclusive), k6-utils semantics.
if (typeof randomIntBetween === 'undefined') {
    var randomIntBetween = function (min, max) {
        return Math.floor(Math.random() * (max - min + 1)) + min;
    };
}

// Random element from an array.
if (typeof randomItem === 'undefined') {
    var randomItem = function (items) {
        return items[Math.floor(Math.random() * items.length)];
    };
}

// Random string of `length` characters from `charset`
// (k6-utils default charset: lowercase a–z).
if (typeof randomString === 'undefined') {
    var randomString = function (length, charset) {
        if (charset === undefined || charset === null || charset === '') {
            charset = 'abcdefghijklmnopqrstuvwxyz';
        }
        var out = '';
        for (var i = 0; i < length; i++) {
            out += charset[Math.floor(Math.random() * charset.length)];
        }
        return out;
    };
}

// RFC 4122 v4 UUID. Random bytes come from Math.random (k6's RNG), not
// the platform crypto, so randomSeed() keeps uuidv4 reproducible.
if (typeof uuidv4 === 'undefined') {
    var uuidv4 = function () {
        var b = [];
        for (var i = 0; i < 16; i++) {
            b[i] = Math.floor(Math.random() * 256);
        }
        b[6] = (b[6] & 0x0f) | 0x40; // version 4
        b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
        var h = function (n) {
            return (n < 16 ? '0' : '') + n.toString(16);
        };
        return (
            h(b[0]) + h(b[1]) + h(b[2]) + h(b[3]) + '-' +
            h(b[4]) + h(b[5]) + '-' +
            h(b[6]) + h(b[7]) + '-' +
            h(b[8]) + h(b[9]) + '-' +
            h(b[10]) + h(b[11]) + h(b[12]) + h(b[13]) + h(b[14]) + h(b[15])
        );
    };
}

// Substring between the first `left`…`right` pair (single string), or
// ALL such substrings when `repeat` is true (k6-utils findBetween).
if (typeof findBetween === 'undefined') {
    var findBetween = function (content, left, right, repeat) {
        var collect = function (src) {
            var start = src.indexOf(left);
            if (start === -1) {
                return [];
            }
            start += left.length;
            var end = src.indexOf(right, start);
            if (end === -1) {
                return [];
            }
            return [src.slice(start, end)].concat(collect(src.slice(end + right.length)));
        };
        var all = collect(String(content));
        if (!repeat) {
            return all.length > 0 ? all[0] : '';
        }
        return all;
    };
}

// ── k6-summary ─────────────────────────────────────────────────────

// Compact k6-style text summary from the handleSummary(data) object
// (shape: crates/tropel-engine/src/summary.rs build_summary_data —
// per-metric { type, contains, values }, top-level thresholds + state).
if (typeof textSummary === 'undefined') {
    var textSummary = function (data, options) {
        var lines = [];
        lines.push('          /\\.  /\\___');
        lines.push('         /  \\/  /  \\  Tropel k6 output');
        lines.push('        /      \\/   \\');
        lines.push('');
        var state = data && data.state ? data.state : {};
        lines.push('     iterations........: ' + String(state.iterations || 0));
        lines.push('     vus..............: ' + String(state.vusMax || 0));
        lines.push('     test duration....: ' + String(state.testRunDurationMs || 0) + 'ms');
        lines.push('     http_reqs........: ' + String(state.http_reqs || 0));
        if (data && data.state) {
            lines.push(
                '     checks............: ' +
                    String(state.checksPassed || 0) + ' passed, ' +
                    String(state.checksFailed || 0) + ' failed'
            );
        }
        lines.push('');
        var metrics = data && data.metrics ? data.metrics : {};
        var names = Object.keys(metrics).sort();
        for (var i = 0; i < names.length; i++) {
            var name = names[i];
            var m = metrics[name];
            var vals = m && m.values ? m.values : {};
            var parts = [];
            var keys = Object.keys(vals);
            for (var j = 0; j < keys.length; j++) {
                var k = keys[j];
                var v = vals[k];
                var unit = '';
                if (m && m.contains === 'time' && typeof v === 'number') {
                    unit = 'ms';
                }
                parts.push(k + '=' + (typeof v === 'number' ? v.toFixed(2) : String(v)) + unit);
            }
            lines.push('     ' + name + '.............: ' + parts.join(' '));
        }
        lines.push('');
        var thresholds = data && data.thresholds ? data.thresholds : {};
        var tnames = Object.keys(thresholds);
        if (tnames.length > 0) {
            lines.push('     thresholds:');
            for (var t = 0; t < tnames.length; t++) {
                lines.push(
                    '       ' + tnames[t] + ' -> ' +
                        (thresholds[tnames[t]] ? 'PASS' : 'FAIL')
                );
            }
            lines.push('');
        }
        return lines.join('\n');
    };
}

// Minimal-but-real HTML report from the handleSummary(data) object: a
// metrics table, thresholds, and run state. Not the full k6 dashboard,
// but a usable artifact (and it RESOLVES — the old behavior deleted the
// import and left htmlReport undefined, so handleSummary silently
// degraded to the default summary with no file and no exit-code change).
if (typeof htmlReport === 'undefined') {
    var htmlReport = function (data) {
        var esc = function (s) {
            return String(s)
                .replace(/&/g, '&amp;')
                .replace(/</g, '&lt;')
                .replace(/>/g, '&gt;')
                .replace(/"/g, '&quot;');
        };
        var state = data && data.state ? data.state : {};
        var metrics = data && data.metrics ? data.metrics : {};
        var thresholds = data && data.thresholds ? data.thresholds : {};
        var names = Object.keys(metrics).sort();

        var rows = '';
        for (var i = 0; i < names.length; i++) {
            var name = names[i];
            var m = metrics[name];
            var vals = m && m.values ? m.values : {};
            var cells = '';
            var keys = Object.keys(vals);
            for (var j = 0; j < keys.length; j++) {
                var k = keys[j];
                var v = vals[k];
                var unit = m && m.contains === 'time' && typeof v === 'number' ? ' ms' : '';
                cells += '<td>' + esc(k) + ': ' +
                    (typeof v === 'number' ? v.toFixed(2) : esc(v)) + esc(unit) + '</td>';
            }
            rows += '<tr><th>' + esc(name) + ' (' + esc(m && m.type) + ')</th>' + cells + '</tr>';
        }

        var trows = '';
        var tnames = Object.keys(thresholds);
        for (var t = 0; t < tnames.length; t++) {
            trows += '<tr><td>' + esc(tnames[t]) + '</td><td>' +
                (thresholds[tnames[t]] ? 'PASS' : 'FAIL') + '</td></tr>';
        }

        return (
            '<!DOCTYPE html><html><head><meta charset="utf-8">' +
            '<title>Tropel k6 report</title>' +
            '<style>body{font-family:sans-serif;margin:2rem}table{border-collapse:collapse;' +
            'margin:1rem 0}th,td{border:1px solid #ccc;padding:4px 8px;text-align:left}' +
            '</style></head><body>' +
            '<h1>Tropel k6 report</h1>' +
            '<p>iterations: ' + esc(state.iterations || 0) + ' · vus: ' +
            esc(state.vusMax || 0) + ' · http_reqs: ' + esc(state.http_reqs || 0) +
            ' · checks: ' + esc(state.checksPassed || 0) + ' passed / ' +
            esc(state.checksFailed || 0) + ' failed · duration: ' +
            esc(state.testRunDurationMs || 0) + ' ms</p>' +
            '<h2>Metrics</h2><table>' + rows + '</table>' +
            (trows ? '<h2>Thresholds</h2><table><tr><th>Expression</th><th>Result</th></tr>' + trows + '</table>' : '') +
            '</body></html>'
        );
    };
}

// ── Stub jslib modules (backlog line 279) ───────────────────────
// httpx and papaparse imports are stripped by the transpiler but had
// no binding, so scripts hit a raw ReferenceError. These stubs throw
// a clear init-time error explaining the limitation.
if (typeof httpx === 'undefined') {
    var httpx = new Proxy({}, {
        get: function(_, prop) {
            if (prop === '__esModule') return false;
            throw new Error('k6/httpx jslib is not supported by Tropel yet (import was stripped). Use k6/http instead.');
        }
    });
}
if (typeof papaparse === 'undefined') {
    var papaparse = new Proxy({}, {
        get: function(_, prop) {
            if (prop === '__esModule') return false;
            throw new Error('k6/papaparse jslib is not supported by Tropel yet (import was stripped). Use a JS CSV parser instead.');
        }
    });
}
