# trp.* API Reference — Tropel Sandbox Scripting

**Canonical binding** · Version `0.1.0` (2026-08-10)

`trp.*` is the canonical Tropel scripting API. It follows Postman convention
(`pm.*` is Postman's sole namespace, so `trp.*` is Tropel's) and is the
primary, documented API. `pm.*` (Postman-compat) and `bru.*` (Bruno-compat)
are frozen peer views over the same shared state.

**Versioning:** This reference is versioned independently of any consumer.
The `trp.*` binding is a semver-committed public API — breaking changes
bump the minor version of the sandbox crate, and the reference doc is tagged
alongside releases.

---

## Variables / Environment

```js
trp.environment.get(key: string) → string | null
trp.environment.set(key: string, value: string)
trp.environment.unset(key: string)
trp.environment.clear()
trp.environment.has(key: string) → boolean
trp.environment.toObject() → object
trp.environment.replaceIn(text: string) → string
```

Scope: environment variables (Postman-compat scope precedence).

---

```js
trp.collectionVariables.get(key: string) → any
trp.collectionVariables.set(key: string, value: any)
trp.collectionVariables.unset(key: string)
trp.collectionVariables.has(key: string) → boolean
trp.collectionVariables.toObject() → object
```

Scope: collection-level variables. Values round-trip JSON-encoded through
the bridge — `JSON.parse` restores the correct type.

---

```js
trp.variables.get(key: string) → any
trp.variables.set(key: string, value: any)
trp.variables.unset(key: string)
trp.variables.replaceIn(text: string) → string
```

Scope: runtime variables (precedence: iteration data > variables > collection
> environment > globals). Values are typed via JSON round-tripping.

---

```js
trp.globals.get(key: string) → string | null
trp.globals.set(key: string, value: string)
trp.globals.unset(key: string)
trp.globals.has(key: string) → boolean
trp.globals.toObject() → object
```

Scope: global variables (lowest precedence).

---

## Request

```js
trp.request.url        → string (read-only)
trp.request.method     → string (read-only)
trp.request.headers    → object (read-only, name → value)
trp.request.header(name: string) → string | null
trp.request.body       → string | null
```

The current request being executed. Read-only during test scripts.

---

## Response

```js
trp.response.code()      → number (status code, e.g. 200)
trp.response.status      → string ("OK")
trp.response.headers     → object (name → value)
trp.response.header(name: string) → string | null
trp.response.body        → string (raw response body)
trp.response.json()      → any (parsed JSON, or throws)
trp.response.text()      → string (body as UTF-8)
trp.response.responseTime → number (ms)
trp.response.cookies     → Cookie[]
trp.response.size        → number (bytes)
```

The completed response from the last request. Throws if no response is
available (e.g. called before the first request completes).

---

## Tests / Assertions

```js
trp.test(name: string, fn: () => void)
// Runs `fn` and records pass/fail as a check metric. If `fn` throws, the
// test is recorded as failed.

trp.expect(actual: any) → Expectation
// Returns a chai-style expectation object:
//   trp.expect(response.code()).to.have.status(200)
//   trp.expect(response.json()).to.have.property('id')
//   trp.expect(body).to.include('...')
//   trp.expect(value).to.equal(expected)
//   trp.expect(value).to.have.lengthOf(n)
//   trp.expect(value).to.be.a('string')
//   trp.expect(value).to.match(/regexp/)
//   trp.expect(value).to.deep.equal(expected)
// Supports `not` modifier: trp.expect(x).not.to.equal(y)
```

---

## Execution / Flow Control

```js
trp.execution.setNextRequest(name: string | null)
// Jump to the named request in the scenario. `null` stops the run.

trp.skipRequest()       // skip the current request (no send, no test script)
trp.skipTests()         // skip the test script for the current request
```

---

## Metrics

```js
trp.metrics.add(name: string, value: number, tags?: object)
// Emit a custom metric sample.

trp.metrics.get(name: string) → Metric | null
// Read the accumulated metric (trend, counter, gauge, or rate).
```

---

## Iteration Data

```js
trp.iterationData.get(key: string) → any
// Read the current iteration's data row (from a CSV / data source).
```

---

## Info / Context

```js
trp.info.iteration          → number (1-based)
trp.info.iterationCount     → number (total iterations in the run)
trp.info.requestName        → string (current request name)
trp.info.scenarioName       → string
trp.info.executorName       → string
trp.info.vuId               → number
```

---

## Groups

```js
trp.group.start(name: string)
trp.group.end()
```

Nested grouping for metrics and output (k6-style group path).

---

## Compatibility

- `trp.*` is the canonical binding — evolves with the product.
- `pm.*` is a frozen Postman 7.56 compat layer. See `TROPEL_PARITY_POSTMAN.md`.
- `bru.*` is a frozen Bruno compat layer. See Bruno's scripting docs.

Product aliases (`<product>.*` → `trp.*`) are set via `SandboxConfig.aliases`.