import { readFileSync } from "node:fs";
import assert from "node:assert/strict";

const source = readFileSync(new URL("../../js/shared/deep-equal.js", import.meta.url), "utf8");
const equal = Function(`${source}\nreturn globalThis.__tropelDeepEqual;`)();

function bytes(...values) {
  return Uint8Array.from(values).buffer;
}

function error(message, detail) {
  const value = new TypeError(message);
  value.detail = detail;
  return value;
}

const cases = [
  ["number", () => 7, () => 7, true],
  ["nan", () => NaN, () => NaN, true],
  ["date", () => new Date(1700000000000), () => new Date(1700000000000), true],
  ["invalid-date", () => new Date(NaN), () => new Date(NaN), true],
  ["regexp", () => /a+b/gi, () => /a+b/ig, true],
  ["array-buffer", () => bytes(1, 2, 3), () => bytes(1, 2, 3), true],
  ["typed-array", () => new Uint16Array([1, 513]), () => new Uint16Array([1, 513]), true],
  ["data-view", () => new DataView(bytes(4, 5, 6)), () => new DataView(bytes(4, 5, 6)), true],
  ["error", () => error("bad", { code: 7 }), () => error("bad", { code: 7 }), true],
  ["map", () => new Map([[{ key: 1 }, [2]]]), () => new Map([[{ key: 1 }, [2]]]), true],
  ["set", () => new Set([{ key: 1 }, 2]), () => new Set([2, { key: 1 }]), true],
  ["object", () => ({ z: [1, { a: true }], a: 2 }), () => ({ a: 2, z: [1, { a: true }] }), true],
  ["promise", () => Promise.resolve(1), () => Promise.resolve(1), false],
  ["date-different", () => new Date(1), () => new Date(2), false],
  ["regexp-different", () => /a/g, () => /a/i, false],
  ["array-buffer-different", () => bytes(1, 2), () => bytes(1, 3), false],
  ["typed-array-different", () => new Uint16Array([1]), () => new Uint16Array([2]), false],
  ["typed-array-type", () => new Uint8Array([1]), () => new Int8Array([1]), false],
  ["data-view-different", () => new DataView(bytes(1)), () => new DataView(bytes(2)), false],
  ["error-different-field", () => error("bad", { code: 7 }), () => error("bad", { code: 8 }), false],
  ["map-different", () => new Map([["a", 1]]), () => new Map([["a", 2]]), false],
  ["set-different", () => new Set([1]), () => new Set([2]), false],
  ["object-different", () => ({ a: 1 }), () => ({ a: 2 }), false],
];

function cycle(value) {
  value.self = value;
  return value;
}

const generated = [];
for (const [name, left, right, expected] of cases) {
  generated.push({ name, family: name.split("-")[0], left: left(), right: right(), expected });
}
generated.push({ name: "plain-cycle", family: "plain-cycle", left: cycle({ value: 1 }), right: cycle({ value: 1 }), expected: true });
const mapCycle = { name: "map-cycle", family: "map-cycle", left: new Map(), right: new Map(), expected: true };
mapCycle.left.set("self", mapCycle.left);
mapCycle.right.set("self", mapCycle.right);
generated.push(mapCycle);
const setCycle = { name: "set-cycle", family: "set-cycle", left: new Set(), right: new Set(), expected: true };
setCycle.left.add(setCycle.left);
setCycle.right.add(setCycle.right);
generated.push(setCycle);

for (const pair of generated) {
  assert.equal(equal(pair.left, pair.right), pair.expected, pair.name);
  assert.equal(equal(pair.right, pair.left), pair.expected, `${pair.name} symmetry`);
}

// Exhaustively check cross-category pairs against the independent oracle:
// distinct supported categories must stay unequal. Same-category semantics
// are asserted by the explicit generated pairs above.
for (const left of generated) {
  for (const right of generated) {
    if (left.family && right.family && left.family === right.family) continue;
    assert.equal(equal(left.left, right.left), false, `${left.name} vs ${right.name}`);
  }
}

console.log(`PASS: deep-equal generated-pair regression (${generated.length} values)`);
