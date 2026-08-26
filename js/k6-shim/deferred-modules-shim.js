// ══════════════════════════════════════════════════════════════════
// TR-245: Deferred modules — k6/websockets, k6/html, k6/net/grpc,
// k6/experimental/{csv,fs,streams}
// ══════════════════════════════════════════════════════════════════
//
// These modules were deferred in the original W2 pass ("full module,
// own PR"). Each is a complete JS implementation of the k6 API surface,
// using native bridges only where necessary (WebSocket transport, file
// reads, gRPC calls). Pure-JS surfaces (html, csv, streams) use no
// native bridges.
//
// Import pattern: `import { parseHTML } from 'k6/html'` is stripped by
// the transpiler; the named export is provided as a bare global here.
// Each module's object is also set as a global so `import grpc from
// 'k6/net/grpc'` (default export) works.
//
// Conventions match the rest of the k6 shim: top-level guarded `var`
// declarations (first definition wins), no IIFE, comments explain why.

// ──────────────────────────────────────────────────────────────────────
// k6/html — parseHTML + Selection (39 methods)
// ──────────────────────────────────────────────────────────────────────
// A lightweight in-JS DOM parser and jQuery-like selection API. k6 uses
// goquery (which wraps golang.org/x/net/html). This implementation
// tokenizes HTML into a tree of {tag, attrs, children, text} nodes and
// provides the Selection surface from k6/js/modules/k6/html/html.go:
// Add, Find, Single, Closest, Has, Not, Next, NextAll, Prev, PrevAll,
// Parent, Parents, Siblings, PrevUntil, NextUntil, ParentsUntil, Size,
// End, Eq, First, Last, Contents, Text, Attr, Html, Val, Children, Each,
// Filter, Is, Map, Slice, Get, ToArray, Index, Data, serialize,
// serializeArray. Loaded AFTER k6-shim.js, so K6Response is defined.

if (typeof parseHTML !== 'function') {

var HTML_VOID_ELEMENTS = {
    area: true, base: true, br: true, col: true, embed: true,
    hr: true, img: true, input: true, link: true, meta: true,
    param: true, source: true, track: true, wbr: true
};

function HtmlNode(type, tag, attrs, children, text) {
    this.type = type || 'element'; // 'element' | 'void' | 'text' | 'doc'
    this.tag = tag || '';
    this.attrs = attrs || {};
    this.children = children || [];
    this.text = text || '';
    this.parent = null;
}

function tokenizeHtml(src) {
    var doc = new HtmlNode('doc', '#document', {}, [], '');
    var stack = [doc];
    var pos = 0;
    var len = src.length;

    function advance() { return pos < len ? src[pos++] : ''; }
    function peek() { return pos < len ? src[pos] : ''; }
    function match(re) {
        var m = src.slice(pos).match(re);
        if (m) { pos += m[0].length; return m[0]; }
        return null;
    }
    function skipWs() {
        while (pos < len && /\s/.test(src[pos])) pos++;
    }

    function parseAttrs() {
        var attrs = {};
        while (pos < len) {
            skipWs();
            if (peek() === '>' || peek() === '/') break;
            var name = match(/^[a-zA-Z_:][a-zA-Z0-9_:.-]*/);
            if (!name) break;
            var value = '';
            skipWs();
            if (peek() === '=') {
                advance(); // =
                skipWs();
                var q = advance();
                if (q === '"' || q === "'") {
                    var end = src.indexOf(q, pos);
                    value = end >= 0 ? src.slice(pos, end) : src.slice(pos);
                    if (end >= 0) pos = end + 1;
                } else {
                    pos--;
                    value = match(/^[^\s>]+/) || '';
                }
            }
            attrs[name.toLowerCase()] = value;
        }
        return attrs;
    }

    function parseTag() {
        advance(); // <
        if (peek() === '/') {
            advance();
            var tag = (match(/^[a-zA-Z][a-zA-Z0-9]*/) || '').toLowerCase();
            match(/^[^>]*>/);
            return { type: 'close', tag: tag };
        }
        if (peek() === '!') {
            advance();
            if (src.slice(pos, pos + 2) === '--') {
                var ci = src.indexOf('-->', pos + 2);
                if (ci >= 0) { pos = ci + 3; } else { pos = len; }
                return { type: 'comment' };
            }
            match(/^[^>]*>/);
            return { type: 'comment' };
        }
        var tag = (match(/^[a-zA-Z][a-zA-Z0-9]*/) || '').toLowerCase();
        var attrs = parseAttrs();
        var selfClosing = false;
        if (peek() === '/') { advance(); selfClosing = true; }
        advance(); // >
        if (selfClosing || HTML_VOID_ELEMENTS[tag]) {
            return { type: 'void', tag: tag, attrs: attrs };
        }
        return { type: 'open', tag: tag, attrs: attrs };
    }

    while (pos < len) {
        if (peek() === '<') {
            var tok = parseTag();
            if (tok.type === 'comment') continue;
            var current = stack[stack.length - 1];
            if (tok.type === 'open') {
                var node = new HtmlNode('element', tok.tag, tok.attrs, [], '');
                node.parent = current;
                current.children.push(node);
                stack.push(node);
            } else if (tok.type === 'close') {
                if (stack.length > 1) {
                    for (var si = stack.length - 1; si >= 0; si--) {
                        if (stack[si].tag === tok.tag) {
                            stack.splice(si, stack.length - si);
                            break;
                        }
                    }
                    if (stack.length === 0) stack.push(doc);
                }
            } else if (tok.type === 'void') {
                var node = new HtmlNode('void', tok.tag, tok.attrs, [], '');
                node.parent = current;
                current.children.push(node);
            }
        } else {
            var text = match(/^[^<]+/);
            if (text !== null) {
                var current = stack[stack.length - 1];
                var tn = new HtmlNode('text', '', {}, [], text);
                tn.parent = current;
                current.children.push(tn);
            }
        }
    }
    return doc;
}

function splitSelector(s) {
    var depth = 0, parts = [], start = 0;
    for (var i = 0; i < s.length; i++) {
        var c = s[i];
        if (c === '(' || c === '[') depth++;
        else if (c === ')' || c === ']') depth--;
        else if (c === ',' && depth === 0) {
            parts.push(s.slice(start, i));
            start = i + 1;
        }
    }
    parts.push(s.slice(start));
    return parts;
}

function parseSimpleSelector(s, start) {
    var tag = '*', id = null, classes = [], attrs = [], pseudo = null;
    var i = start;
    var m = s.slice(i).match(/^([a-zA-Z*][a-zA-Z0-9]*)/);
    if (m) { tag = m[1]; i += m[0].length; }
    while (i < s.length) {
        if (s[i] === '#') {
            i++;
            var idm = s.slice(i).match(/^[a-zA-Z_][a-zA-Z0-9_-]*/);
            if (idm) { id = idm[0]; i += idm[0].length; }
        } else if (s[i] === '.') {
            i++;
            var cm = s.slice(i).match(/^[a-zA-Z_][a-zA-Z0-9_-]*/);
            if (cm) { classes.push(cm[0]); i += cm[0].length; }
        } else if (s[i] === '[') {
            i++;
            var an = s.slice(i).match(/^[a-zA-Z_][a-zA-Z0-9_-]*/);
            if (!an) break;
            var attrName = an[0].toLowerCase(); i += an[0].length;
            var op = 'exists', attrVal = '';
            if (s[i] === '~' && s[i + 1] === '=') { op = '~='; i += 2; }
            else if (s[i] === '|' && s[i + 1] === '=') { op = '|='; i += 2; }
            else if (s[i] === '^' && s[i + 1] === '=') { op = '^='; i += 2; }
            else if (s[i] === '$' && s[i + 1] === '=') { op = '$='; i += 2; }
            else if (s[i] === '*' && s[i + 1] === '=') { op = '*='; i += 2; }
            else if (s[i] === '=') { op = '='; i += 1; }
            if (op !== 'exists') {
                var q = s[i];
                var endA = q === '"' || q === "'" ? s.indexOf(q, i + 1) : -1;
                if (endA >= 0) { attrVal = s.slice(i + 1, endA); i = endA + 1; }
                else { var av = s.slice(i).match(/^[^\]]+/); if (av) { attrVal = av[0]; i += av[0].length; } }
            }
            i++; // ]
            attrs.push({ name: attrName, op: op, value: attrVal });
        } else if (s[i] === ':') {
            i++;
            var pm = s.slice(i).match(/^[a-zA-Z][a-zA-Z0-9-]*/);
            if (pm) { pseudo = pm[0].toLowerCase(); i += pm[0].length; }
            else break;
        } else break;
    }
    return { consumed: i - start, tag: tag, id: id, classes: classes, attrs: attrs, pseudo: pseudo };
}

function matchSimple(node, simple) {
    if (!node || (node.type !== 'element' && node.type !== 'void')) return false;
    if (simple.tag !== '*' && node.tag !== simple.tag) return false;
    if (simple.id && node.attrs.id !== simple.id) return false;
    for (var ci = 0; ci < simple.classes.length; ci++) {
        var cls = node.attrs['class'];
        if (!cls || (' ' + cls + ' ').indexOf(' ' + simple.classes[ci] + ' ') < 0) return false;
    }
    for (var ai = 0; ai < simple.attrs.length; ai++) {
        var a = simple.attrs[ai];
        var val = node.attrs[a.name];
        if (a.op === 'exists' && val === undefined) return false;
        if (a.op === '=' && val !== a.value) return false;
        if (a.op === '~=' && (' ' + (val || '') + ' ').indexOf(' ' + a.value + ' ') < 0) return false;
        if (a.op === '|=' && val !== a.value && val !== a.value + '-') return false;
        if (a.op === '^=' && (val || '').indexOf(a.value) !== 0) return false;
        if (a.op === '$=' && (val || '').indexOf(a.value) !== (val || '').length - a.value.length) return false;
        if (a.op === '*=' && (val || '').indexOf(a.value) < 0) return false;
    }
    return true;
}

function matchBySelector(node, selector) {
    if (typeof selector !== 'string') return false;
    var parts = splitSelector(selector);
    for (var pi = 0; pi < parts.length; pi++) {
        var simple = parseSimpleSelector(parts[pi].trim(), 0);
        if (matchSimple(node, simple)) return true;
    }
    return false;
}

function getAllDescendants(node) {
    var result = [];
    for (var i = 0; i < node.children.length; i++) {
        var child = node.children[i];
        if (child.type === 'element' || child.type === 'void') {
            result.push(child);
            result = result.concat(getAllDescendants(child));
        }
    }
    return result;
}

function getChildElements(node) {
    var result = [];
    for (var i = 0; i < node.children.length; i++) {
        if (node.children[i].type === 'element' || node.children[i].type === 'void') {
            result.push(node.children[i]);
        }
    }
    return result;
}

function getPrevSiblings(node) {
    var result = [];
    if (!node.parent) return result;
    for (var i = 0; i < node.parent.children.length; i++) {
        var c = node.parent.children[i];
        if (c === node) break;
        if (c.type === 'element' || c.type === 'void') result.push(c);
    }
    return result;
}

function getNextSiblings(node) {
    var result = [];
    if (!node.parent) return result;
    var found = false;
    for (var i = 0; i < node.parent.children.length; i++) {
        var c = node.parent.children[i];
        if (c === node) { found = true; continue; }
        if (found && (c.type === 'element' || c.type === 'void')) result.push(c);
    }
    return result;
}

function findInTree(node, simple) {
    var result = [];
    if (matchSimple(node, simple)) result.push(node);
    for (var i = 0; i < node.children.length; i++) {
        result = result.concat(findInTree(node.children[i], simple));
    }
    return result;
}

function selectNodes(root, selector) {
    if (!selector || typeof selector !== 'string') return [];
    var s = selector.trim();
    if (s === '') return [];
    var parts = splitSelector(s);
    var results = [];
    for (var pi = 0; pi < parts.length; pi++) {
        results = results.concat(selectCompound(root, parts[pi].trim()));
    }
    return dedupeNodes(results);
}

function selectCompound(root, sel) {
    // Tokenize combinators: descendant (space), child (>), adjacent (+), sibling (~)
    var tokens = [];
    var i = 0, len = sel.length;
    while (i < len) {
        if (/\s/.test(sel[i])) {
            i++;
            var next = i;
            while (next < len && /\s/.test(sel[next])) next++;
            if (next < len && sel[next] !== '>' && sel[next] !== '+' && sel[next] !== '~') {
                tokens.push({ combinator: 'descendant' });
            }
            continue;
        }
        if (sel[i] === '>') { i++; tokens.push({ combinator: 'child' }); continue; }
        if (sel[i] === '+') { i++; tokens.push({ combinator: 'adjacent' }); continue; }
        if (sel[i] === '~') { i++; tokens.push({ combinator: 'sibling' }); continue; }
        var simple = parseSimpleSelector(sel, i);
        tokens.push({ combinator: 'match', simple: simple });
        i += simple.consumed;
    }
    if (tokens.length === 0) return [];
    return walkTokens(root, tokens, 0);
}

function walkTokens(root, tokens, tokIdx) {
    var tok = tokens[tokIdx];
    if (tok.combinator !== 'match') return [];
    var candidates = [];
    if (tokIdx === 0) {
        if (matchSimple(root, tok.simple)) candidates.push(root);
        for (var di = 0; di < root.children.length; di++) {
            candidates = candidates.concat(findInTree(root.children[di], tok.simple));
        }
    } else {
        var prev = walkTokens(root, tokens, tokIdx - 1);
        var comb = tokens[tokIdx - 1].combinator;
        for (var pi = 0; pi < prev.length; pi++) {
            var pc = prev[pi];
            if (comb === 'descendant') candidates = candidates.concat(getAllDescendants(pc));
            else if (comb === 'child') candidates = candidates.concat(getChildElements(pc));
            else if (comb === 'adjacent') {
                var ns = getNextSiblings(pc);
                if (ns.length > 0) candidates.push(ns[0]);
            } else if (comb === 'sibling') candidates = candidates.concat(getNextSiblings(pc));
        }
    }
    return candidates.filter(function (n) { return matchSimple(n, tok.simple); });
}

function dedupeNodes(nodes) {
    var seen = [], result = [];
    for (var i = 0; i < nodes.length; i++) {
        if (seen.indexOf(nodes[i]) < 0) { seen.push(nodes[i]); result.push(nodes[i]); }
    }
    return result;
}

function nodeText(n) {
    var out = '';
    for (var i = 0; i < n.children.length; i++) {
        var c = n.children[i];
        if (c.type === 'text') out += c.text;
        else out += nodeText(c);
    }
    return out;
}

function nodeInnerHtml(n) {
    var out = '';
    for (var i = 0; i < n.children.length; i++) out += nodeOuterHtml(n.children[i]);
    return out;
}

function nodeOuterHtml(n) {
    if (n.type === 'text') return n.text;
    if (n.type === 'comment') return '';
    var attrs = '';
    for (var ak in n.attrs) {
        if (n.attrs.hasOwnProperty(ak)) {
            attrs += ' ' + ak + '="' + String(n.attrs[ak]).replace(/"/g, '&quot;') + '"';
        }
    }
    if (HTML_VOID_ELEMENTS[n.tag] || n.type === 'void') {
        return '<' + n.tag + attrs + '/>';
    }
    return '<' + n.tag + attrs + '>' + nodeInnerHtml(n) + '</' + n.tag + '>';
}

function selToElement(sel) {
    var n = sel._nodes.length > 0 ? sel._nodes[0] : null;
    if (!n) return null;
    var el = {
        _node: n,
        node: n,
        nodeName: n.tag,
        nodeType: 1,
        innerHtml: function () { return nodeInnerHtml(n); },
        outerHtml: function () { return nodeOuterHtml(n); },
        textContent: nodeText(n),
        text: function () { return nodeText(n); },
        html: function () { return nodeInnerHtml(n); },
        attr: function (name) { return n.attrs[name] || null; },
        serialize: function () { return serializeForm(sel._root); },
        serializeArray: function () { return serializeFormArray(sel._root); },
        value: function () { return n.attrs.value || n.text || ''; },
        is: function (selector) { return matchBySelector(n, selector); }
    };
    return el;
}

function propertyToAttr(name) {
    return name.replace(/[A-Z]/g, function (c) { return '-' + c.toLowerCase(); });
}

function attrToProperty(name) {
    return name.replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); });
}

function convertDataAttrVal(val) {
    var n = Number(val);
    if (!isNaN(n) && String(n) === val) return n;
    if (val === 'true') return true;
    if (val === 'false') return false;
    if (val === 'null') return null;
    try { return JSON.parse(val); } catch (e) { return val; }
}

function serializeForm(root) {
    var parts = [];
    (function walk(n) {
        for (var i = 0; i < n.children.length; i++) {
            var c = n.children[i];
            if (c.tag === 'input' || c.tag === 'textarea' || c.tag === 'select') {
                var name = c.attrs.name;
                if (!name) continue;
                var val = '';
                if (c.tag === 'textarea') val = nodeText(c);
                else if (c.tag === 'select') {
                    var opts = findInTree(c, { tag: '*', id: null, classes: [], attrs: [], pseudo: null });
                    for (var oi = 0; oi < opts.length; oi++) {
                        if (opts[oi].attrs.selected !== undefined || oi === 0) {
                            val = opts[oi].attrs.value !== undefined ? opts[oi].attrs.value : nodeText(opts[oi]);
                            break;
                        }
                    }
                } else {
                    var t = c.attrs.type || 'text';
                    if (t === 'radio' || t === 'checkbox') {
                        if (c.attrs.checked !== undefined) val = c.attrs.value || 'on';
                    } else val = c.attrs.value || '';
                }
                parts.push(encodeURIComponent(name) + '=' + encodeURIComponent(val));
            }
            walk(c);
        }
    })(root);
    return parts.join('&');
}

function serializeFormArray(root) {
    var arr = [];
    (function walk(n) {
        for (var i = 0; i < n.children.length; i++) {
            var c = n.children[i];
            if (c.tag === 'input' || c.tag === 'textarea' || c.tag === 'select') {
                var name = c.attrs.name;
                if (!name) continue;
                var val = '';
                if (c.tag === 'textarea') val = nodeText(c);
                else if (c.tag === 'select') {
                    var opts = findInTree(c, { tag: '*', id: null, classes: [], attrs: [], pseudo: null });
                    var chosen = false;
                    for (var oi = 0; oi < opts.length; oi++) {
                        if (opts[oi].attrs.selected !== undefined || oi === 0) {
                            arr.push({ name: name, value: opts[oi].attrs.value !== undefined ? opts[oi].attrs.value : nodeText(opts[oi]) });
                            chosen = true;
                            break;
                        }
                    }
                    if (!chosen) arr.push({ name: name, value: '' });
                } else {
                    var t = c.attrs.type || 'text';
                    if (t === 'radio' || t === 'checkbox') {
                        if (c.attrs.checked !== undefined) arr.push({ name: name, value: c.attrs.value || 'on' });
                    } else arr.push({ name: name, value: c.attrs.value || '' });
                }
            }
            walk(c);
        }
    })(root);
    return arr;
}

function HtmlSelection(root, nodes, url, prev) {
    this._root = root;
    this._nodes = nodes || [];
    this.length = this._nodes.length;
    this.url = url || '';
    this._prev = prev || null;
}

HtmlSelection.prototype = {
    constructor: HtmlSelection,

    Add: function (arg) {
        var newNodes = this._nodes.slice();
        if (arg instanceof HtmlSelection) newNodes = newNodes.concat(arg._nodes);
        else if (typeof arg === 'string') newNodes = newNodes.concat(selectNodes(this._root, arg));
        return new HtmlSelection(this._root, dedupeNodes(newNodes), this.url);
    },

    Find: function (arg) {
        if (arg instanceof HtmlSelection) {
            var newNodes = [];
            for (var i = 0; i < this._nodes.length; i++) newNodes = newNodes.concat(getAllDescendants(this._nodes[i]));
            return new HtmlSelection(this._root, dedupeNodes(newNodes), this.url);
        }
        var sel = String(arg || '');
        var newNodes = [];
        for (var i = 0; i < this._nodes.length; i++) {
            newNodes = newNodes.concat(selectNodes(this._nodes[i], sel));
        }
        return new HtmlSelection(this._root, dedupeNodes(newNodes), this.url);
    },

    Single: function (selector) {
        var sel = selectNodes(this._root, selector);
        if (sel.length > 1) sel = sel.slice(0, 1);
        return new HtmlSelection(this._root, sel, this.url);
    },

    Closest: function (arg) {
        var result = [];
        var simple = typeof arg === 'string' ? parseSimpleSelector(arg, 0) : null;
        var targetNodes = arg instanceof HtmlSelection ? arg._nodes : null;
        for (var i = 0; i < this._nodes.length; i++) {
            var n = this._nodes[i];
            while (n && n.type !== 'doc') {
                var matched = simple ? matchSimple(n, simple) :
                    targetNodes ? targetNodes.indexOf(n) >= 0 : false;
                if (matched) { result.push(n); break; }
                n = n.parent;
            }
        }
        return new HtmlSelection(this._root, dedupeNodes(result), this.url);
    },

    Has: function (arg) {
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) {
            var n = this._nodes[i];
            var descendants = getAllDescendants(n);
            var found = false;
            if (arg instanceof HtmlSelection) {
                for (var di = 0; di < descendants.length; di++) {
                    if (arg._nodes.indexOf(descendants[di]) >= 0) { found = true; break; }
                }
            } else if (typeof arg === 'string') {
                found = selectNodes(n, arg).length > 0;
            }
            if (found) result.push(n);
        }
        return new HtmlSelection(this._root, result, this.url);
    },

    Not: function (arg) {
        var exclude = [];
        if (typeof arg === 'function') {
            var kept = [];
            for (var i = 0; i < this._nodes.length; i++) {
                var s = new HtmlSelection(this._root, [this._nodes[i]], this.url);
                if (!arg(i, selToElement(s))) kept.push(this._nodes[i]);
            }
            return new HtmlSelection(this._root, kept, this.url);
        }
        if (arg instanceof HtmlSelection) exclude = arg._nodes;
        else if (typeof arg === 'string') {
            for (var i = 0; i < this._nodes.length; i++) {
                exclude = exclude.concat(selectNodes(this._root, arg));
            }
        }
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) {
            if (exclude.indexOf(this._nodes[i]) < 0) result.push(this._nodes[i]);
        }
        return new HtmlSelection(this._root, result, this.url);
    },

    _adjacent: function (dir, def) {
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) {
            var sibs = dir === 'next' ? getNextSiblings(this._nodes[i]) :
                       dir === 'prev' ? getPrevSiblings(this._nodes[i]) : [];
            if (dir === 'prev' && sibs.length > 0) result.push(sibs[sibs.length - 1]);
            else if (dir === 'next' && sibs.length > 0) result.push(sibs[0]);
        }
        if (def !== undefined && def !== null) {
            var simple = parseSimpleSelector(String(def), 0);
            result = result.filter(function (n) { return matchSimple(n, simple); });
        }
        return new HtmlSelection(this._root, dedupeNodes(result), this.url);
    },

    _adjacentAll: function (dir, def) {
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) {
            result = result.concat(dir === 'next' ? getNextSiblings(this._nodes[i]) : getPrevSiblings(this._nodes[i]));
        }
        if (def !== undefined && def !== null) {
            var simple = parseSimpleSelector(String(def), 0);
            result = result.filter(function (n) { return matchSimple(n, simple); });
        }
        return new HtmlSelection(this._root, dedupeNodes(result), this.url);
    },

    Next: function (def) { return this._adjacent('next', def); },
    NextAll: function (def) { return this._adjacentAll('next', def); },
    Prev: function (def) { return this._adjacent('prev', def); },
    PrevAll: function (def) { return this._adjacentAll('prev', def); },

    Parent: function (def) {
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) {
            if (this._nodes[i].parent && this._nodes[i].parent.type !== 'doc') {
                result.push(this._nodes[i].parent);
            }
        }
        if (def !== undefined && def !== null) {
            var simple = parseSimpleSelector(String(def), 0);
            result = result.filter(function (n) { return matchSimple(n, simple); });
        }
        return new HtmlSelection(this._root, dedupeNodes(result), this.url);
    },

    Parents: function (def) {
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) {
            var n = this._nodes[i].parent;
            while (n && n.type !== 'doc') { result.push(n); n = n.parent; }
        }
        if (def !== undefined && def !== null) {
            var simple = parseSimpleSelector(String(def), 0);
            result = result.filter(function (n) { return matchSimple(n, simple); });
        }
        return new HtmlSelection(this._root, dedupeNodes(result), this.url);
    },

    Siblings: function (def) {
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) {
            result = result.concat(getPrevSiblings(this._nodes[i])).concat(getNextSiblings(this._nodes[i]));
        }
        if (def !== undefined && def !== null) {
            var simple = parseSimpleSelector(String(def), 0);
            result = result.filter(function (n) { return matchSimple(n, simple); });
        }
        return new HtmlSelection(this._root, dedupeNodes(result), this.url);
    },

    _until: function (dir, args) {
        var until = args[0];
        var filter = args[1];
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) {
            var stop = function (n) {
                if (until === undefined || until === null) return false;
                if (typeof until === 'string') return matchBySelector(n, until);
                if (until instanceof HtmlSelection) return until._nodes.indexOf(n) >= 0;
                return false;
            };
            var collected = [];
            if (dir === 'parent') {
                var n = this._nodes[i].parent;
                while (n && n.type !== 'doc') { if (stop(n)) break; collected.push(n); n = n.parent; }
            } else if (dir === 'next') {
                var sibs = getNextSiblings(this._nodes[i]);
                for (var si = 0; si < sibs.length; si++) { if (stop(sibs[si])) break; collected.push(sibs[si]); }
            } else {
                var sibs = getPrevSiblings(this._nodes[i]);
                for (var si = sibs.length - 1; si >= 0; si--) { if (stop(sibs[si])) break; collected.unshift(sibs[si]); }
            }
            result = result.concat(collected);
        }
        if (filter !== undefined && filter !== null) {
            var simple = parseSimpleSelector(String(filter), 0);
            result = result.filter(function (n) { return matchSimple(n, simple); });
        }
        return new HtmlSelection(this._root, dedupeNodes(result), this.url);
    },

    PrevUntil: function () { return this._until('prev', arguments); },
    NextUntil: function () { return this._until('next', arguments); },
    ParentsUntil: function () { return this._until('parent', arguments); },

    Size: function () { return this.length; },

    End: function () { return this._prev || new HtmlSelection(this._root, [this._root], this.url); },

    Eq: function (idx) {
        var i = idx < 0 ? this._nodes.length + idx : idx;
        var nodes = (i >= 0 && i < this._nodes.length) ? [this._nodes[i]] : [];
        return new HtmlSelection(this._root, nodes, this.url, this);
    },

    First: function () {
        var nodes = this._nodes.length > 0 ? [this._nodes[0]] : [];
        return new HtmlSelection(this._root, nodes, this.url, this);
    },

    Last: function () {
        var nodes = this._nodes.length > 0 ? [this._nodes[this._nodes.length - 1]] : [];
        return new HtmlSelection(this._root, nodes, this.url, this);
    },

    Contents: function () {
        var nodes = [];
        for (var i = 0; i < this._nodes.length; i++) nodes = nodes.concat(this._nodes[i].children);
        return new HtmlSelection(this._root, nodes, this.url);
    },

    Text: function () {
        var out = '';
        for (var i = 0; i < this._nodes.length; i++) out += nodeText(this._nodes[i]);
        return out;
    },

    Attr: function (name, def) {
        if (this._nodes.length === 0) return def !== undefined ? def : undefined;
        var val = this._nodes[0].attrs[name];
        if (val === undefined) return def !== undefined ? def : undefined;
        return val;
    },

    Html: function () {
        if (this._nodes.length === 0) return undefined;
        return nodeInnerHtml(this._nodes[0]);
    },

    Val: function () {
        if (this._nodes.length === 0) return undefined;
        var n = this._nodes[0];
        var tag = n.tag;
        if (tag === 'input') {
            var t = (n.attrs.type || 'text').toLowerCase();
            var v = n.attrs.value;
            if (v !== undefined) return v;
            if (t === 'radio' || t === 'checkbox') return 'on';
            return '';
        }
        if (tag === 'button') return n.attrs.value || '';
        if (tag === 'textarea') return nodeText(n);
        if (tag === 'option') return n.attrs.value !== undefined ? n.attrs.value : nodeText(n);
        if (tag === 'select') {
            if (n.attrs.multiple !== undefined) {
                var result = [];
                for (var ci = 0; ci < n.children.length; ci++) {
                    var opt = n.children[ci];
                    if (opt.attrs && opt.attrs.selected !== undefined) {
                        result.push(opt.attrs.value !== undefined ? opt.attrs.value : nodeText(opt));
                    }
                }
                return result;
            }
            var selected = findInTree(n, { tag: '*', id: null, classes: [], attrs: [{ name: 'selected', op: 'exists', value: '' }], pseudo: null });
            if (selected.length === 0) selected = findInTree(n, { tag: '*', id: null, classes: [], attrs: [], pseudo: null });
            if (selected.length > 0) return selected[0].attrs.value !== undefined ? selected[0].attrs.value : nodeText(selected[0]);
            return '';
        }
        return undefined;
    },

    Children: function (def) {
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) result = result.concat(getChildElements(this._nodes[i]));
        if (def !== undefined && def !== null) {
            var simple = parseSimpleSelector(String(def), 0);
            result = result.filter(function (n) { return matchSimple(n, simple); });
        }
        return new HtmlSelection(this._root, dedupeNodes(result), this.url);
    },

    Each: function (fn) {
        if (typeof fn !== 'function') throw new Error('the argument to each() must be a function');
        for (var i = 0; i < this._nodes.length; i++) {
            var sel = new HtmlSelection(this._root, [this._nodes[i]], this.url);
            fn(i, selToElement(sel));
        }
        return this;
    },

    Filter: function (v) {
        if (typeof v === 'string') {
            var simple = parseSimpleSelector(v, 0);
            return new HtmlSelection(this._root, this._nodes.filter(function (n) { return matchSimple(n, simple); }), this.url);
        }
        if (v instanceof HtmlSelection) {
            return new HtmlSelection(this._root, this._nodes.filter(function (n) { return v._nodes.indexOf(n) >= 0; }), this.url);
        }
        if (typeof v === 'function') {
            var self = this;
            return new HtmlSelection(this._root, this._nodes.filter(function (n, i) {
                return v(i, selToElement(new HtmlSelection(self._root, [n], self.url)));
            }), this.url);
        }
        throw new Error('the argument to filter() must be a function, a selector or a selection');
    },

    Is: function (v) {
        if (typeof v === 'string') {
            return this._nodes.length > 0 && matchBySelector(this._nodes[0], v);
        }
        if (v instanceof HtmlSelection) {
            return this._nodes.length > 0 && v._nodes.indexOf(this._nodes[0]) >= 0;
        }
        if (typeof v === 'function') {
            return this._nodes.length > 0 && !!v(0, selToElement(new HtmlSelection(this._root, [this._nodes[0]], this.url)));
        }
        return false;
    },

    Map: function (fn) {
        if (typeof fn !== 'function') throw new Error('the argument to map() must be a function');
        var result = [];
        for (var i = 0; i < this._nodes.length; i++) {
            var sel = new HtmlSelection(this._root, [this._nodes[i]], this.url);
            var val = fn(i, selToElement(sel));
            if (val !== undefined) result.push(val);
        }
        return result;
    },

    Slice: function (start, end) {
        var nodes = this._nodes.slice(start, end !== undefined ? end : this._nodes.length);
        return new HtmlSelection(this._root, nodes, this.url);
    },

    Get: function (idx) {
        if (idx === undefined) {
            var items = [];
            for (var i = 0; i < this._nodes.length; i++) {
                items.push(selToElement(new HtmlSelection(this._root, [this._nodes[i]], this.url)));
            }
            return items;
        }
        var i = idx < 0 ? this._nodes.length + idx : idx;
        if (i >= 0 && i < this._nodes.length) {
            return selToElement(new HtmlSelection(this._root, [this._nodes[i]], this.url));
        }
        return undefined;
    },

    ToArray: function () {
        var arr = [];
        for (var i = 0; i < this._nodes.length; i++) arr.push(new HtmlSelection(this._root, [this._nodes[i]], this.url));
        return arr;
    },

    Index: function (def) {
        if (def === undefined) {
            if (!this._nodes[0] || !this._nodes[0].parent) return -1;
            return getChildElements(this._nodes[0].parent).indexOf(this._nodes[0]);
        }
        if (def instanceof HtmlSelection) return def._nodes.length > 0 ? this._nodes.indexOf(def._nodes[0]) : -1;
        if (typeof def === 'string') {
            if (this._nodes.length > 0) {
                var simple = parseSimpleSelector(def, 0);
                var sibs = this._nodes[0].parent ? getChildElements(this._nodes[0].parent) : [];
                for (var i = 0; i < sibs.length; i++) {
                    if (matchSimple(sibs[i], simple)) {
                        if (sibs[i] === this._nodes[0]) return i;
                    }
                }
            }
            return -1;
        }
        return -1;
    },

    Data: function (name) {
        if (this._nodes.length === 0) return undefined;
        var n = this._nodes[0];
        if (name !== undefined) {
            var val = n.attrs['data-' + propertyToAttr(name)];
            if (val === undefined) return undefined;
            return convertDataAttrVal(val);
        }
        var data = {};
        for (var ak in n.attrs) {
            if (n.attrs.hasOwnProperty(ak) && ak.indexOf('data-') === 0 && ak.length > 5) {
                data[attrToProperty(ak.slice(5))] = convertDataAttrVal(n.attrs[ak]);
            }
        }
        return data;
    },

    serialize: function () { return serializeForm(this._root); },
    serializeArray: function () { return serializeFormArray(this._root); }
};

var parseHTML = function (src) {
    var doc = tokenizeHtml(String(src || ''));
    return new HtmlSelection(doc, doc.children, '');
};

// Upgrade res.html() from the old regex stub to the full Selection.
K6Response.prototype.html = function (selector) {
    var body = typeof this.body === 'string' ? this.body : '';
    var sel = parseHTML(body);
    if (selector !== undefined) {
        return sel.Find(String(selector));
    }
    return sel;
};

var k6html = { parseHTML: parseHTML };
}

// ──────────────────────────────────────────────────────────────────────
// k6/experimental/csv — csv.parse returns an array of rows with both
// Symbol.iterator and Symbol.asyncIterator (k6's own is incomplete; the
// async iterator is what scripts actually need).
// ──────────────────────────────────────────────────────────────────────
if (typeof csv === 'undefined') {
var csv = {};

csv.parse = function (fileContent, options) {
    options = options || {};
    var skipFirstLine = options.skipFirstLine === true;
    var delimiter = options.delimiter || ',';
    var lines = String(fileContent || '').split(/\r?\n/);
    var parsed = [];
    for (var i = skipFirstLine ? 1 : 0; i < lines.length; i++) {
        var line = lines[i];
        if (line.trim() === '') continue;
        var fields = [];
        var current = '';
        var inQuotes = false;
        for (var j = 0; j < line.length; j++) {
            var c = line[j];
            if (c === '"') {
                if (inQuotes && j + 1 < line.length && line[j + 1] === '"') {
                    current += '"';
                    j++;
                } else {
                    inQuotes = !inQuotes;
                }
            } else if (c === delimiter && !inQuotes) {
                fields.push(current);
                current = '';
            } else {
                current += c;
            }
        }
        fields.push(current);
        parsed.push(fields);
    }
    // Indexable + both iterators. Indexing matches SharedArray semantics;
    // Symbol.asyncIterator is what k6's incomplete csv lacks.
    parsed[Symbol.iterator] = function () {
        var idx = 0;
        return { next: function () { return idx < parsed.length ? { value: parsed[idx++], done: false } : { done: true }; } };
    };
    parsed[Symbol.asyncIterator] = function () {
        var idx = 0;
        return {
            next: function () {
                return Promise.resolve(idx < parsed.length ? { value: parsed[idx++], done: false } : { done: true });
            }
        };
    };
    return parsed;
};
}

// ──────────────────────────────────────────────────────────────────────
// k6/experimental/streams — ReadableStream + reader + controller
// ──────────────────────────────────────────────────────────────────────
if (typeof ReadableStream === 'undefined') {

function ReadableStreamDefaultController(stream) {
    this._stream = stream;
}

ReadableStreamDefaultController.prototype = {
    constructor: ReadableStreamDefaultController,
    enqueue: function (chunk) { this._stream._enqueue(chunk); },
    close: function () { this._stream._closeStream(); },
    error: function (e) { this._stream._errorStream(e); }
};

function ReadableStreamDefaultReader(stream) {
    this._stream = stream;
}

ReadableStreamDefaultReader.prototype = {
    constructor: ReadableStreamDefaultReader,

    read: function () {
        var self = this;
        return new Promise(function (resolve) {
            if (self._stream._chunks.length > 0) {
                resolve({ value: self._stream._chunks.shift(), done: false });
            } else if (self._stream._state === 'closed') {
                resolve({ done: true });
            } else {
                self._stream._readRequests.push(resolve);
            }
        });
    },

    cancel: function (reason) { return this._stream.cancel(reason); },

    releaseLock: function () {
        this._stream._lock = null;
        this._stream._readers--;
    }
};

var ReadableStream = function (underlyingSource) {
    this._state = 'readable'; // 'readable' | 'closed' | 'errored'
    this._chunks = [];
    this._readers = 0;
    this._readRequests = [];
    this._error = null;
    this._lock = null;
    this._pull = null;
    this._cancelFn = null;
    this._controller = new ReadableStreamDefaultController(this);

    if (underlyingSource) {
        var self = this;
        if (typeof underlyingSource.start === 'function') {
            try { underlyingSource.start(this._controller); } catch (e) { this._errorStream(e); }
        }
        if (typeof underlyingSource.pull === 'function') {
            this._pull = function () { underlyingSource.pull(self._controller); };
        }
        if (typeof underlyingSource.cancel === 'function') {
            this._cancelFn = function (reason) { underlyingSource.cancel(reason); };
        }
    }
};

ReadableStream.prototype = {
    constructor: ReadableStream,

    get locked() { return this._lock !== null; },

    getReader: function () {
        if (this._lock) throw new TypeError('ReadableStream is already locked');
        var reader = new ReadableStreamDefaultReader(this);
        this._lock = reader;
        this._readers++;
        return reader;
    },

    cancel: function (reason) {
        if (this._lock) return Promise.reject(new TypeError('Cannot cancel while locked'));
        if (this._cancelFn) this._cancelFn(reason);
        this._state = 'closed';
        this._chunks = [];
        return Promise.resolve();
    },

    pipeTo: function (dest) {
        var self = this;
        var reader = this.getReader();
        function pump() {
            return reader.read().then(function (result) {
                if (result.done) { return dest.close(); }
                var writer = dest.getWriter();
                return writer.write(result.value).then(function () {
                    writer.releaseLock();
                    return pump();
                });
            });
        }
        return pump();
    },

    pipeThrough: function (transform) {
        this.pipeTo(transform.writable);
        return transform.readable;
    },

    tee: function () {
        var reader = this.getReader();
        var branches = [new ReadableStream(), new ReadableStream()];
        function pump() {
            reader.read().then(function (result) {
                if (result.done) {
                    branches[0]._closeStream();
                    branches[1]._closeStream();
                    return;
                }
                branches[0]._enqueue(result.value);
                branches[1]._enqueue(result.value);
                pump();
            });
        }
        pump();
        return branches;
    },

    _enqueue: function (chunk) {
        if (this._state !== 'readable') throw new TypeError('Cannot enqueue on a closed/errored stream');
        this._chunks.push(chunk);
        // Resolve pending read requests
        while (this._chunks.length > 0 && this._readRequests.length > 0) {
            var resolve = this._readRequests.shift();
            resolve({ value: this._chunks.shift(), done: false });
        }
    },

    _closeStream: function () {
        this._state = 'closed';
        while (this._readRequests.length > 0) this._readRequests.shift()({ done: true });
    },

    _errorStream: function (e) {
        this._state = 'errored';
        this._error = e;
        while (this._readRequests.length > 0) this._readRequests.shift()(Promise.reject(e));
    }
};
}

// ──────────────────────────────────────────────────────────────────────
// k6/experimental/fs — File (open/read/seek/stat/size/close)
// Uses the native __tropel_k6_fs_open bridge when present (driver.rs);
// falls back to the open-data-shim's open(path, 'b') sync read.
// ──────────────────────────────────────────────────────────────────────
if (typeof fs === 'undefined') {
var fs = {};

fs.SeekMode = { Start: 0, Current: 1, End: 2 };

fs.open = function (path) {
    if (typeof __tropel_k6_fs_open === 'function') {
        return __tropel_k6_fs_open(String(path));
    }
    // Fallback: reuse the open-data-shim's open(path, 'b') sync read.
    var data = open(String(path), 'b');
    if (data === undefined || data === null) {
        throw new Error('fs.open: file not found: ' + path);
    }
    var bytes = data;
    if (bytes instanceof ArrayBuffer) bytes = new Uint8Array(bytes);
    if (typeof bytes === 'string') {
        var enc = [];
        for (var si = 0; si < bytes.length; si++) enc.push(bytes.charCodeAt(si) & 0xFF);
        bytes = enc;
    }
    var pos = 0;
    return {
        size: bytes.length,
        read: function (n) {
            if (pos >= bytes.length) return null;
            var end = Math.min(pos + n, bytes.length);
            var chunk = bytes.slice(pos, end);
            pos = end;
            return chunk;
        },
        seek: function (offset, whence) {
            whence = whence === undefined ? 0 : whence;
            if (whence === 0) pos = offset;
            else if (whence === 1) pos += offset;
            else if (whence === 2) pos = bytes.length + offset;
            if (pos < 0) pos = 0;
            if (pos > bytes.length) pos = bytes.length;
            return pos;
        },
        stat: function () {
            return { name: String(path), size: bytes.length, mode: 0, mtime: null, isdir: false };
        },
        close: function () { /* no-op for fallback */ }
    };
};
}

// ──────────────────────────────────────────────────────────────────────
// k6/websockets — WHATWG WebSocket + Blob
// Delegates to the same native ws bridges as the legacy k6/ws (they share
// one tokio_tungstenite session backend). The modern API is event-driven
// (addEventListener / on*), so a script that sleeps or yields after
// sending triggers the message pump. k6 has no removeEventListener — but
// providing it is harmless and matches the WHATWG spec, so it stays.
// ──────────────────────────────────────────────────────────────────────
if (typeof WebSocket === 'undefined') {

var WS_CONNECTING = 0, WS_OPEN = 1, WS_CLOSING = 2, WS_CLOSED = 3;

var Blob = function (parts, opts) {
    opts = opts || {};
    var data = '';
    for (var i = 0; i < (parts || []).length; i++) {
        var p = parts[i];
        if (p instanceof ArrayBuffer) {
            var view = new Uint8Array(p);
            for (var bi = 0; bi < view.length; bi++) data += String.fromCharCode(view[bi]);
        } else if (p instanceof Blob) {
            data += p._text;
        } else {
            data += String(p);
        }
    }
    this._text = data;
    this.size = data.length;
    this.type = opts.type || '';
};

Blob.prototype = {
    constructor: Blob,
    slice: function (start, end, type) {
        return new Blob([this._text.slice(start || 0, end || this.size)], { type: type || this.type });
    },
    text: function () { return Promise.resolve(this._text); },
    arrayBuffer: function () {
        var buf = new ArrayBuffer(this._text.length);
        var view = new Uint8Array(buf);
        for (var i = 0; i < this._text.length; i++) view[i] = this._text.charCodeAt(i) & 0xFF;
        return Promise.resolve(buf);
    }
};

var WebSocket = function (url, protocols) {
    this.url = String(url);
    this.protocols = protocols || [];
    this.readyState = WS_CONNECTING;
    this.bufferedAmount = 0;
    this.extensions = '';
    this.protocol = '';
    this.binaryType = 'blob'; // 'blob' | 'arraybuffer'
    this._listeners = {};
    this._sessionId = null;
    this._closed = false;
    this._pendingEvents = [];

    if (typeof __tropel_k6_ws_connect === 'function') {
        var self = this;
        try {
            this._sessionId = __tropel_k6_ws_connect(this.url, '{}');
            self.readyState = WS_OPEN;
            // Register for the sleep() event pump.
            if (typeof self._register === 'function') self._register();
            self._pumpEvents();
            self._dispatchEvent('open', { type: 'open', target: self });
        } catch (e) {
            self.readyState = WS_CLOSED;
            self._dispatchEvent('error', { type: 'error', target: self, message: String(e) });
        }
    }
};

WebSocket.prototype = {
    constructor: WebSocket,

    addEventListener: function (type, listener) {
        if (typeof listener !== 'function') return;
        if (!this._listeners[type]) this._listeners[type] = [];
        this._listeners[type].push(listener);
    },

    removeEventListener: function (type, listener) {
        var arr = this._listeners[type];
        if (!arr) return;
        this._listeners[type] = arr.filter(function (l) { return l !== listener; });
    },

    _dispatchEvent: function (type, event) {
        var arr = this._listeners[type];
        if (arr) {
            for (var i = 0; i < arr.length; i++) {
                try { arr[i](event); } catch (e) { /* swallow listener errors */ }
            }
        }
        var handler = this['on' + type];
        if (typeof handler === 'function') {
            try { handler(event); } catch (e) { /* swallow listener errors */ }
        }
    },

    // Drain pending native events synchronously. Called from send() and
    // from the shim's sleep() hook so the common script pattern
    // "send then sleep then read messages" works without an event loop.
    _pumpEvents: function () {
        if (!this._sessionId || typeof __tropel_k6_ws_step !== 'function') return;
        var self = this;
        var maxPumps = 100;
        while (maxPumps-- > 0 && !self._closed) {
            var evt;
            try {
                evt = JSON.parse(__tropel_k6_ws_step(self._sessionId, 0));
            } catch (e) {
                break;
            }
            if (!evt || !evt.type) break;
            if (evt.type === 'message') {
                self._dispatchEvent('message', { type: 'message', data: evt.data, target: self });
            } else if (evt.type === 'ping') {
                self._dispatchEvent('ping', { type: 'ping', data: evt.data, target: self });
            } else if (evt.type === 'pong') {
                self._dispatchEvent('pong', { type: 'pong', data: evt.data, target: self });
            } else if (evt.type === 'error') {
                self._dispatchEvent('error', { type: 'error', message: evt.message || '', target: self });
            } else if (evt.type === 'close') {
                self.readyState = WS_CLOSED;
                self._closed = true;
                self._dispatchEvent('close', { type: 'close', code: evt.code || 1000, reason: evt.reason || '', target: self });
                break;
            }
        }
    },

    send: function (data) {
        if (this.readyState !== WS_OPEN) throw new Error('WebSocket is not open: readyState=' + this.readyState);
        if (typeof __tropel_k6_ws_send === 'function') {
            var payload = data;
            if (data instanceof ArrayBuffer) payload = data;
            else if (data instanceof Blob) payload = data._text;
            else payload = String(data);
            __tropel_k6_ws_send(this._sessionId, payload);
            this.bufferedAmount = 0;
        }
        // A send may elicit a reply — pump once.
        this._pumpEvents();
    },

    close: function (code, reason) {
        if (this.readyState === WS_CLOSED || this.readyState === WS_CLOSING) return;
        this.readyState = WS_CLOSING;
        if (typeof __tropel_k6_ws_close === 'function') {
            __tropel_k6_ws_close(this._sessionId, code || 1000, reason || '');
        }
        this.readyState = WS_CLOSED;
        this._closed = true;
        this._dispatchEvent('close', { type: 'close', code: code || 1000, reason: reason || '', target: this });
    }
};

// WhatwgBlob and WebSocket are exposed as globals for `new WebSocket(...)`.
var WebSocketConstructor = WebSocket;

// TR-245: a registry of live WebSocket instances so sleep() (which has no
// event loop to fire handlers) can pump pending events. Every socket
// registers in the constructor and unregisters on close.
var __tropel_live_websockets = [];

WebSocket.prototype._register = function () {
    if (__tropel_live_websockets.indexOf(this) < 0) {
        __tropel_live_websockets.push(this);
    }
};

var __origWsClose = WebSocket.prototype.close;
WebSocket.prototype.close = function (code, reason) {
    __origWsClose.call(this, code, reason);
    var idx = __tropel_live_websockets.indexOf(this);
    if (idx >= 0) __tropel_live_websockets.splice(idx, 1);
};

// Pump all live sockets. Called by the shim's sleep() before and after the
// blocking native sleep, so server-pushed messages surface.
function __tropel_websocket_pump_all() {
    var list = __tropel_live_websockets.slice();
    for (var i = 0; i < list.length; i++) {
        try { list[i]._pumpEvents(); } catch (e) { /* swallow */ }
    }
}
}

// ──────────────────────────────────────────────────────────────────────
// k6/net/grpc — Client + Status/HealthCheck constants
// Delegates to the native __tropel_k6_grpc_* bridges when available
// (driver.rs registers them); otherwise throws a clear error rather than
// silently doing nothing (invariant 3: never declare a capability that
// isn't forwarded).
// ──────────────────────────────────────────────────────────────────────
if (typeof grpc === 'undefined') {
var grpc = {};

// 17 Status* constants (k6/js/modules/k6/net/grpc/constants.go)
grpc.StatusOK = 0;
grpc.StatusCanceled = 1;
grpc.StatusUnknown = 2;
grpc.StatusInvalidArgument = 3;
grpc.StatusDeadlineExceeded = 4;
grpc.StatusNotFound = 5;
grpc.StatusAlreadyExists = 6;
grpc.StatusPermissionDenied = 7;
grpc.StatusResourceExhausted = 8;
grpc.StatusFailedPrecondition = 9;
grpc.StatusAborted = 10;
grpc.StatusOutOfRange = 11;
grpc.StatusUnimplemented = 12;
grpc.StatusInternal = 13;
grpc.StatusUnavailable = 14;
grpc.StatusDataLoss = 15;
grpc.StatusUnauthenticated = 16;

// 4 HealthCheck* constants (k6 ships the typo "Unkown")
grpc.HealthCheckServiceUnknown = 0;
grpc.HealthCheckServiceUnkown = 0; // k6's shipped typo, kept for parity
grpc.HealthCheckServing = 1;
grpc.HealthCheckNotServing = 2;

grpc.Client = function () {
    this._addr = null;
    this._plaintext = false;
    this._loaded = [];
    this._protoSrc = null;
    this._protoDir = null;
    this._conn = false;
};

// protoPathToDir: /a/b/hello.proto → /a/b
function protoPathToDir(p) {
    var idx = p.lastIndexOf('/');
    return idx > 0 ? p.slice(0, idx) : '.';
}

grpc.Client.prototype = {
    constructor: grpc.Client,

    // k6: load(imports, ...protoPaths) — init-only. Reads each proto file
    // (via the k6 open() bridge) and keeps the LAST source + the first
    // file's directory (so relative imports resolve). The bridge compiles
    // and caches the pool; the register notes k6's loadProtoset is an
    // alternate loader — the protoset path is stored and treated as a
    // proto source file.
    load: function (imports, protoPaths) {
        var paths = [];
        if (typeof protoPaths === 'string') paths.push(protoPaths);
        else if (Array.isArray(protoPaths)) paths = paths.concat(protoPaths);
        if (typeof imports === 'string') paths.push(imports);
        else if (Array.isArray(imports)) paths = paths.concat(imports);
        for (var i = 0; i < paths.length; i++) {
            var p = paths[i];
            if (typeof p !== 'string' || p === '') continue;
            this._loaded.push(p);
            var content = open(p, 'b');
            var src = '';
            if (typeof content === 'string') {
                src = content;
            } else if (content instanceof ArrayBuffer) {
                var view = new Uint8Array(content);
                for (var bi = 0; bi < view.length; bi++) src += String.fromCharCode(view[bi]);
            } else if (content && content.byteLength !== undefined) {
                for (var bi2 = 0; bi2 < content.byteLength; bi2++) src += String.fromCharCode(content[bi2]);
            }
            if (src !== '') {
                this._protoSrc = src;
                if (!this._protoDir) this._protoDir = protoPathToDir(p);
            }
        }
        return this;
    },

    loadProtoset: function (protosetPath) {
        if (typeof protosetPath === 'string' && protosetPath !== '') {
            this._loaded.push(protosetPath);
            var content = open(protosetPath, 'b');
            var src = '';
            if (typeof content === 'string') src = content;
            else if (content instanceof ArrayBuffer) {
                var view = new Uint8Array(content);
                for (var bi = 0; bi < view.length; bi++) src += String.fromCharCode(view[bi]);
            }
            if (src !== '') this._protoSrc = src;
            if (!this._protoDir) this._protoDir = protoPathToDir(protosetPath);
        }
        return this;
    },

    connect: function (addr, options) {
        options = options || {};
        this._addr = addr;
        this._plaintext = options.plaintext === true;
        this._conn = true;
        return this;
    },

    invoke: function (method, request, params) {
        params = params || {};
        var timeout = params.timeout || 120000; // k6 default: 2 min
        if (typeof __tropel_k6_grpc_invoke !== 'function') {
            throw new Error('k6/net/grpc: invoke() requires the native gRPC bridge, which is not available in this runtime');
        }
        if (!this._protoSrc && this._protoSrc !== '') {
            throw new Error('k6/net/grpc: client.load() must be called with a proto file before invoke()');
        }
        var result = JSON.parse(__tropel_k6_grpc_invoke(
            this._addr,
            method,
            typeof request === 'string' ? request : JSON.stringify(request),
            this._plaintext,
            timeout,
            this._protoSrc || '',
            this._protoDir || ''
        ));
        return {
            status: result.status !== undefined ? result.status : grpc.StatusOK,
            headers: result.headers || {},
            trailers: result.trailers || {},
            error: result.error !== undefined ? result.error : null,
            error_code: result.error_code !== undefined ? result.error_code : 0,
            message: result.message !== undefined ? result.message : null,
            response: result.response !== undefined ? result.response : null
        };
    },

    stream: function (method, request, params) {
        throw new Error('k6/net/grpc: streaming (client.stream) is not supported');
    },

    close: function () {
        this._conn = false;
        this._addr = null;
        return this;
    }
};
}
