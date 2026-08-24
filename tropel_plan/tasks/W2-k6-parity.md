# W2 · k6 parity — semantics, then surface

**Gate:** a real k6 script runs unmodified and produces **k6's** numbers.

Reference: `grafana/k6` @ `53b5727`, **v2.1.0**, read first-hand. Tags used below: **[GAP]** no answer · **[SILENT]** differs in a way that breaks a real k6 script without saying so · **[SUPERSET]** already better — keep it.

**Net standing:** on executors and outputs tropel is a **superset** of k6 v2 — `externally-controlled` and StatsD were both deleted upstream and tropel kept them. The real gaps are the **JS module surface**, **metric/tag fidelity**, and **three specific algorithms**.

Parity means *a k6 script gets k6's answer*. It does not mean copying k6's bugs — where tropel is better (deterministic multipart ordering, escaped field names), keep it and document it.

Source: `TROPEL_PARITY_K6.md`, `TROPEL_PARITY_POSTMAN.md`, `TROPEL_MASTER_TODO.md` §W6, §W3.

---

# Track A — Make the silent things loud

## TR-201 · Warn on every unrecognized option
**Effort:** S · **Blocked by:** none · **Do this first**

**The cheapest item in either parity document.** ~20 options are accepted and silently ignored.

> **◐ PARTIAL — verified at `2099cbe`.** The **scenario** config already has it: `tropel-sdk/src/config.rs:66` is `#[serde(tag = "type", deny_unknown_fields)]`, and `:1105` records *"a typo'd camelCase option is a hard error, not"* a silent drop. That half is done. **The k6 *root* options below live in a different struct and are still silently dropped** — re-scope this task to those.

- [x] Scenario config rejects unknown keys
- [ ] `#[serde(flatten)] unknown: HashMap<String, Value>` on the **k6 root options**, plus a warning per unrecognized key
- [ ] Converts a whole silent class into a visible one in one change
- [ ] Notable ignored today: `minIterationDuration`, `userAgent` (k6 default `Grafana k6/<version>`), `batch`/`batchPerHost`, `tags`, `throw`, `setupTimeout`/`teardownTimeout`, `systemTags`, `summaryTimeUnit`, `tlsAuth`/`tlsVersion`/`tlsCipherSuites`, `blockHostnames`, `httpDebug`, `maxRedirects`, `metricSamplesBufferSize`, `noCookiesReset`, `ext`/`cloud`
- [ ] Note the deliberate divergence: **k6 itself has no `default:` branch on `Params`** and silently discards misspelled keys. Warning is better; document that it is intentional

---

# Track B — The metric and tag contract

This is what makes a k6 dashboard pointed at tropel *correct* rather than plausible.

## TR-202 · `http_req_duration` is not wall-clock time
**Effort:** M · **Blocked by:** TR-011

**[SILENT] · The highest-impact single line in the parity document.**

k6 defines it as **`sending + waiting + receiving`**, deliberately excluding `blocked`, `connecting` and `tls_handshaking` (`lib/netext/httpext/tracer.go:381`). If tropel sums wall-clock, **every duration threshold ported from a k6 script is wrong by one connection setup** — and wrong in the direction that hides regressions on a warm pool.

- [x] Adopt the exact formula
- [ ] `sending` and `tls_handshaking` stop being hardcoded 0 — both are real Trends *and* real `res.timings` fields, and because `duration` includes `sending`, hardcoding it **deflates `http_req_duration`**
- [ ] Port the three subtleties from `tracer.go`: the reused-connection stamp overwrite (`:271-293`), the TLS-vs-plain `sending` basis selection (`:346-359`), and the `gotFirstResponseByte > wroteRequest` guard (`:364`) that prevents a negative `waiting` on HTTP/2
- [ ] A conformance fixture runs the same script through k6 and tropel against one server and asserts the durations agree within tolerance

## TR-203 · Emit all 8 HTTP metrics on transport failure
**Effort:** M · **Blocked by:** none · **Same fix as TR-121**

**[SILENT]** k6's `measureAndEmitMetrics` has **no error branch**; `SaveSamples` is called unconditionally (`transport.go:144`). A DNS failure never fires `GotConn`/`WroteRequest`, so every timing records a genuine **`0`** — the sample exists, it is just zero. Dropping them makes `http_reqs` and `http_req_duration` counts disagree and biases every percentile.

## TR-204 · `error`, `error_code` and `expected_response` tags
**Effort:** M · **Blocked by:** TR-203

- [x] **[SILENT]** `error_code` for a non-2xx is **`1000 + status`** (404 → 1404) while the `error` tag stays **empty**. Only transport errors populate `error`. Reimplementations routinely invert this
- [x] **[GAP]** Full `error_code` enumeration: 1000 generic, 1010 non-TCP, 1020 invalid URL, 1050 timeout, 1100/1101 DNS, 1110/1111 blacklist/blocked, 1200–1220 TCP, 1301/1310/1311 TLS, 1000+status for ≥400, 1611–1664 HTTP/2, 1701 decompression
- [ ] **Do not implement 1300 and 1600** — declared upstream but unreachable
- [x] Without `error_code` there is no way to distinguish "connection refused" from "504" in aggregate

## TR-205 · The `url` tag is overwritten with `name`
**Effort:** S · **Blocked by:** none · **Blocks:** TR-206

**[SILENT]** Since k6 v0.41, at emit time: if `name` was set, **`url` is assigned the `name` value** (`transport.go:87-101`, asserted in `request_test.go:61`). They are *always identical*, precisely so a high-cardinality URL cannot leak into the series space. Tropel sets both to the full resolved URL — which is the cardinality blowup in the backlog.

## TR-206 · `http.url` tagged template
**Effort:** M · **Blocked by:** TR-205

**[GAP] This is k6's entire cardinality-control mechanism** (`http.go:182-193`). It builds `name` with each interpolation replaced by the literal `${}`, collapsing `/users/1`, `/users/2`, … into one series `/users/${}`. `tags: {name: "getUser"}` does it manually.

- [ ] Without it, tropel's own `MAX_SERIES` cap does the user's cardinality management by dropping data

## TR-207 · Real group paths
**Effort:** S · **Blocked by:** TR-133

**[SILENT]** The `group` tag is hardcoded `"http"`. k6's is the `k6/group` nesting path with a **leading `::`** — `group("a")` → `"::a"`, nested → `"::a::b"`, root is `""` (`lib/models.go:25,162-170`). A name containing `::` is a hard error. `setup`/`teardown` are tagged `"::setup"`/`"::teardown"`.

## TR-208 · The `--out json` schema
**Effort:** S · **Blocked by:** none

**[SILENT]** k6 emits (`internal/output/json/wrapper.go`):

```json
{"metric":"http_reqs","type":"Point","data":{"time":"…","value":1,"tags":{…},"metadata":{…}}}
{"type":"Metric","metric":"http_req_duration","data":{"name":"…","type":"trend","contains":"time","thresholds":[…],"submetrics":[…]}}
```

Tropel emits the **InfluxDB point shape** (`data.measurement`, `data.fields.value`) with no top-level `metric`, so **every k6-ecosystem consumer reads null**. `metric` is at the top level of *both* record types.

## TR-209 · `__ITER` is 0-based; `__VU` is 0 during init
**Effort:** S · **Blocked by:** none

**[SILENT]** `iteration` seeds at `-1` and is incremented before use (`runner.go:223,860`); `__VU` is 1-based only *during iterations* (`lib/execution.go:239`). Tropel is 1-based for both, so every `data[__ITER % len]` and `users[__VU-1]` partitioning script is silently off by one.

- [ ] Also: `__ITER` truncates to `i32` (`driver.rs:3068-3072`) — past 2³¹ iterations the modulo indexes negatively

## TR-210 · `data_sent` / `data_received` are connection-level
**Effort:** M · **Blocked by:** none

**[SILENT]** k6 takes them from the Dialer's byte counters — **per-iteration, not per-request**, and they include headers. Tropel counts request-body bytes only, so **a GET-only test reports 0**.

## TR-211 · `vus` / `vus_max` are scheduler-sampled once per second
**Effort:** S · **Blocked by:** none

**[SILENT]** Tropel has every VU emit its own pair every 100 iterations — **~1000 duplicate samples/s at 1000 VUs**, which is both wrong and a measurable egress cost.

## TR-212 · `systemTags` and the indexable/metadata split
**Effort:** M · **Blocked by:** TR-204, TR-207

- [ ] **[GAP]** The option itself, plus k6's default set: `proto, subproto, status, method, url, name, group, check, error, error_code, tls_version, scenario, service, expected_response`
- [ ] `iter` and `vu` exist but are **non-indexable metadata**, deliberately excluded to bound cardinality — copy the distinction, it is the design
- [ ] Off by default: `ocsp_status`, `ip`
- [ ] **[GAP]** `ws_ping` and `ws_connecting` are not emitted; tropel emits a non-k6 `ws_req_duration` and lists `ws_session_duration` in its scaling table without emitting it
- [ ] **[SUPERSET]** Keep StatsD. Keep `externally-controlled`. Both were deleted in k6 v2

---

# Track C — Three algorithms to port, not approximate

## TR-220 · `ramping-vus` — port the absolute-offset step table
**Effort:** L · **Blocked by:** TR-002

**k6 does not act-then-sleep.** It builds a full step table at `Init()` (`ramping_vus.go:482-490`) and sleeps to **absolute offsets**: `diff := offset - time.Since(start)` (`:701-716`), measured from the executor's own start. **Timing error never accumulates across steps.**

Tropel's act-then-sleep leads by one step *and* accumulates drift, and its ramp-down re-arms surplus from the stage-start count and can overshoot to zero VUs. **A precomputed absolute-offset table makes both bugs structurally impossible** — port the model, don't patch the symptoms.

- [ ] Interpolation rules (`:194-233`): `stageVUDiff == 0` → **HOLD, no steps at all**; `stageDuration == 0` → instant `GoTo`; ramp-down walks the index backwards with spacing `timeTillEnd - stageDuration*(stageEndVUs-unscaled+1)/stageVUDiff`
- [ ] Defaults: `startVUs` 1, `gracefulRampDown` **30 s**, `gracefulStop` **30 s**, `maxConcurrentVUs = 100_000_000`
- [ ] Scenario names must match `^[0-9a-zA-Z_-]+$`
- [ ] A `1s` floor applies to `duration`/`maxDuration`, but **0-duration stages are legal**

## TR-221 · Arrival rate — striped offsets and rational segment scaling
**Effort:** L · **Blocked by:** TR-220

`constant_arrival_rate.go:316-330` walks a **global** iteration index `gi`, recomputing the deadline as `period × gi` from `time.Since(startTime)` every tick — **zero drift**. Segmentation is expressed purely by *skipping the global ticks this segment doesn't own*, so N instances interleave into the exact original global rate.

- [ ] **[GAP]** Implement `GetStripedOffsets`. Tropel runs an *independent* arrival-rate executor per node, so arrivals **bunch instead of interleaving**
- [ ] **[SILENT]** Segment scaling must use exact rationals — k6 uses `big.Rat`; tropel's `f64` gives 100 agents / 100 VUs → **two agents get 0 VUs and two get 2**
- [ ] Ramping arrival rate integrates the rate curve in closed form (`ramping_arrival_rate.go:234-283`), solving a quadratic for linear ramps and carrying the fractional remainder across stage boundaries via `doneSoFar`
- [ ] **[GAP, deliberate]** `rps` is **not** segment-scaled in k6 either — tropel's 4-agents-each-enforcing-the-full-cap **matches**. Document it as a shared footgun; do not "fix" it into a divergence

## TR-222 · Thresholds — grammar, semantics, and the units model
**Effort:** M · **Blocked by:** TR-112

Grammar (`metrics/thresholds_parser.go`): aggregations **`value`, `count`, `rate`, `avg`, `min`, `med`, `max`, `p(N)`**; operators **`<=`, `<`, `>=`, `>`, `===`, `==`, `!=`**.

- [ ] **[SILENT]** `value` (Gauge-only) is unsupported and **aborts the run at startup**. Same for `===`. Same for compound `&&`/`||` — the evaluator supports them, only the translator doesn't
- [ ] **[SILENT]** Counter `rate` means **per-second**; tropel returns the total for `http_reqs` and a per-series mean for custom counters
- [ ] **[SILENT]** An unknown stat must be a **startup error**, never a silent resolve to the mean
- [ ] **[SILENT]** Tag-scoped thresholds never match — the key renderer and the matcher disagree on format
- [x]  A threshold whose tag value contains a space currently kills the run at startup — fix in the same pass
- [ ] **[GAP]** Adopt `metrics/units.go`'s model (`Time` = ms, `Data` = bytes, `Default`). This is the clean fix for the systemic µs/ms confusion. Note it is byte-identical to k6 v1.8 — ancient k6, not a v2 feature
- [ ] Threshold expressions are re-parsed every tick and it runs twice — cache while you are here

---

# Track D — The `k6/http` surface

## TR-230 · `Params` — the missing fields
**Effort:** L · **Blocked by:** TR-201

**[GAP]** Tropel drops `auth`, `redirects`, `compression`, `cookies`, `jar`, `throw`, `responseCallback`, and `responseType:"binary"`.

- [ ] `headers` — a `Host` key sets `req.Host`; a user `Content-Length` is **deleted with a warning**
- [ ] `cookies` — including the `{value, replace}` form; `replace:false`, the default, sends **both** the request cookie and the jar cookie
- [ ] `tags` — setting `tags.name` overrides **both** `name` and `url` (see `TR-205`)
- [ ] `auth` — `"digest"`/`"ntlm"`; **`"basic"` is a documented no-op**, basic auth works purely from URL userinfo
- [ ] **[SILENT]** `timeout` default is **60 s**, not "no timeout"; a number means ms, a string is a duration
- [ ] `redirects` default 10; **`0` returns the 3xx** rather than erroring
- [ ] `compression` — `gzip,deflate,zstd,br`, applied left-to-right
- [ ] `responseType` — `text` / `binary`→ArrayBuffer / `none`
- [ ] **[SILENT]** `params.headers` in Postman array form is silently dropped (`k6-shim.js:184` requires a non-Array) ✅**EXEC**
- [ ] **[SILENT]** `http.post(url, {obj})` sends JSON; k6 sends **form-urlencoded** ✅closed — keep the regression test

## TR-231 · `Response` — the missing members
**Effort:** L · **Blocked by:** TR-014, TR-204

**[GAP]** Missing: `res.cookies`, `res.request`, `res.error`, `res.error_code`, `res.remote_ip`/`remote_port`, `res.proto`, `res.tls_*`, `res.ocsp`, `res.html()`, `res.submitForm()`, `res.clickLink()`, `timings.looking_up`.

- [ ] `status` is **0 on transport error**
- [ ] `body` is **`null` for 1xx/204/304 regardless of `responseType`** — the common `.json()` crash when porting
- [ ] `headers` use Go-canonical MIME keys, multi-values **`", "`-joined into one string**; `request.headers` values are **arrays**, unlike `res.headers`
- [ ] `cookies` shape `{name: [{name,value,domain,path,http_only,secure,max_age,expires}]}`, `expires` in **Unix ms**
- [ ] `request` is **pre-flight, first hop only**
- [ ] `json(selector?)` uses **gjson** paths, not JSONPath. The no-selector form **throws** with a line/char annotation and caches; the selector form returns **`undefined`** on bad JSON or a missing path
- [ ] `timings.looking_up` is declared by k6 and **never assigned** — emit 0 for byte-compat

## TR-232 · `http.file()` and multipart
**Effort:** M · **Blocked by:** TR-230

- [ ] `http.file(data, filename?, contentType?)`; the multipart trigger is "any top-level body value is FileData"
- [ ] 60-hex-char random boundary; file parts carry `Content-Disposition` + `Content-Type`; **non-file parts get no `Content-Type`** and are stringified with `%v`
- [ ] Non-file object bodies fall back to `application/x-www-form-urlencoded`, arrays expanding to repeated keys
- [ ] **[SUPERSET]** k6's part order is **non-deterministic** (Go map range) and it **doesn't escape the field name** for file parts — real k6 bugs. If tropel is deterministic and escapes both, **keep it and document the divergence**

## TR-233 · Cookie jar, response callbacks, and the rest of the module
**Effort:** M · **Blocked by:** TR-230

- [ ] `http.cookieJar()` / `new http.CookieJar()` — 4 methods; `cookiesForURL` returns **values only**; `set` parses `expires` as **RFC1123**; `clear`/`delete` work by re-setting `MaxAge=-1`
- [ ] `http.setResponseCallback` / `http.expectedStatuses` — **default range 200–399 inclusive**; `null` suppresses `http_req_failed` entirely (pairs with `TR-004`)
- [ ] `http.asyncRequest`, `http.head`, `http.options`, and the `TLS_1_*` / `OCSP_*` constants
- [ ] **[GAP]** `batch` per-host limiter — `batch`=20 global, `batchPerHost`=6; the return container mirrors the input; only the **first** error surfaces; GET/HEAD bodies nulled in object form
- [ ] **[GAP]** `del` takes a body; `get`/`head` do not

---

# Track E — The rest of the JS surface

## TR-240 · HTTP inside `setup()` and `teardown()`
**Effort:** L · **Blocked by:** none

**P0 [GAP] — the biggest single JS-surface gap.** k6 runs both in a **full transient VU with real VU state** (`runner.go:640`), so `http.*`, `check`, metrics and groups all work, tagged `group: "::setup"`/`"::teardown"`. **"Log in during setup, pass the token to every VU" is the single most common k6 idiom**, and tropel's setup cannot make HTTP calls at all.

- [ ] The setup return value is **JSON round-tripped** (`runner.go:303-308`) — functions, Symbols, Maps and circular refs are dropped or error; `undefined` means "no data"
- [ ] `setupTimeout`/`teardownTimeout` default **60 s**; `handleSummaryTimeout` **120 s**

## TR-241 · `k6/encoding` and `k6/crypto`
**Effort:** M · **Blocked by:** none

- [ ] **[GAP]** `k6/encoding` — only `b64encode`/`b64decode`, but they appear in a huge fraction of real scripts. `b64decode` returns a **string only when `format === "s"`**, else an ArrayBuffer; unknown encodings silently fall back to `std`
- [ ] **[GAP]** `k6/crypto` **API shape** — scripts write `crypto.sha256(s,'hex')` and `crypto.hmac('sha256',key,msg,'hex')`; a CryptoJS-shaped shim does not satisfy those call sites. 14 functions, five output encodings including **`"binary"` → ArrayBuffer** and `base64rawurl`, plus stateful `createHash`/`createHMAC` → `Hasher{update,digest}`, plus `k6/crypto/x509` (4 functions)
- [x]  Fix the CryptoJS edges in the same pass: `CryptoJS.MD5`/`SHA1` with a cfg object **silently return an empty-key HMAC** — the `SHA256` guard exists and its siblings never got it
- [ ] **[GAP]** WebCrypto is the **global `crypto`** object, not an import path — `import … from 'k6/webcrypto'` fails in real k6 too

## TR-242 · Timers, `randomSeed`, and the globals
**Effort:** S · **Blocked by:** none

- [ ] **[GAP]** `k6/timers` — but these are **globals**; the module is a pure re-export, so implementing the globals is sufficient. It also unblocks the lodash `debounce`/`throttle` shims
- [ ] **[GAP]** `randomSeed()` — one line, and the only way to get a reproducible run. **Per-VU, not global**
- [ ] `__tropel_timers` grows without bound — it reaps only *expired* one-shots, so `setInterval` handles accumulate for the whole run
- [ ] **[SILENT]** `console` has exactly 5 methods — `log`, `debug`, `info`, `warn`, `error`, with **`log` aliasing `info`**. No `trace`/`table`/`group`/`dir`/`time`/`assert`
- [ ] **[GAP]** `TextEncoder`/`TextDecoder` globals
- [ ] 34 generic globals currently leak from `k6-shim.js` — `parse`, `crypto`, `open`, `test`, `hmac`, `randomBytes` — and `globalThis.crypto` is the wrong object. Namespacing them is part of this task, not a follow-up

## TR-243 · `check()`, `group()`, and the metric constructors
**Effort:** M · **Blocked by:** TR-207

- [ ] **[SILENT]** `check()` returns a **plain bool**, never throws on failure; emits the builtin `checks` Rate tagged `check:<name>`; takes a **third `tags` argument**; **rejects async functions**; a throwing check emits a `false` sample *then* propagates
- [ ] `group()` also rejects async callbacks and emits `group_duration`
- [ ] **[SILENT]** `isTime` was dropped from metric constructors. The second arg switches the metric to `ValueType.Time` = **values are milliseconds** — it changes threshold semantics and summary units, not just formatting
- [ ] `.name` is **read-only**; `.add()` **returns a boolean** and does not throw on a bad value unless `options.throw`; metrics **must** be constructed in init context
- [ ] Custom-metric Counter read-back returns the last value, not the total (`trp.rs:1131,1177`)

## TR-244 · `k6/execution` — the mutable tag objects
**Effort:** M · **Blocked by:** TR-212

- [ ] **[GAP]** `exec.vu.tags`, `exec.vu.metrics.tags` and `exec.vu.metrics.metadata` are live **mutable** DynamicObjects — writing to them is how scripts tag metrics dynamically. Everything else on `exec` is getter-only. Values restricted to String/Boolean/Number
- [ ] **[GAP]** `exec.test.abort(msg?)` → exit code **108**, and **`exec.test.fail(msg?)`**, which marks the run failed *without stopping it*. Two distinct things

## TR-245 · The remaining modules, ranked by demand
**Effort:** L · **Blocked by:** TR-241

- [ ] **[GAP]** `k6/websockets` (WHATWG `WebSocket` + `Blob`) alongside legacy `k6/ws`. **Both emit the same six `ws_*` metrics**, so metric parity doesn't tell you which API a script uses. Legacy **blocks the VU** in a callback; modern is event-loop driven. `onping`/`onpong` are k6 extensions; there is **no `removeEventListener`**
- [ ] **[GAP]** `k6/net/grpc` from scripts — note the import path is `k6/net/grpc`, and `k6/experimental/grpc` throws a "graduated" error. `Client` (`load`/`loadProtoset` are **init-only**), `Stream` (the `status` event is registrable but **never emitted**), 17 `Status*` constants, 4 `HealthCheck*` constants including the shipped typo `HealthCheckServiceUnkown`. `invoke` default timeout **2 min**; streams have **no default timeout**
- [ ] **[GAP]** `k6/html` — `parseHTML` + 39 Selection methods. More load-bearing than it sounds: `serializeArray`/`serialize` is a real k6 login idiom
- [ ] **[GAP]** `SharedArray` semantics — builder runs **once per process** under a mutex, elements stored JSON-stringified outside the JS heap, mutation throws `TypeError`, init-only, builder must be sync and return an Array. **[SUPERSET]** tropel's avoids k6's per-access `JSON.parse` + recursive freeze — keep that, match the semantics. Close the view gaps: `filter`, `reduce`, `sort`, `concat` are all `undefined` ✅**EXEC**
- [ ] **[GAP]** `k6/experimental/{csv,fs,streams}` — **k6's own are incomplete** (no `Symbol.asyncIterator` on the CSV parser; no `pipeTo`/`pipeThrough`/`TransformStream`/`tee()` on streams), so parity here is a lower bar than the spec implies
- [ ] **[GAP]** `k6/secrets` (2 methods)
- [ ] **Decide `k6/browser` explicitly** — in or out, but not by omission. Survey only: 9 `browser_*` metrics
- [ ] **[SILENT]** `open()` — mode `"b"` → ArrayBuffer else string; init-only; **files not opened during the `__VU==0` pass cannot be opened later**; directories error

## TR-246 · Working scripts that behave wrongly
**Effort:** M · **Blocked by:** none

Distinct from the gaps above: these break a script that k6 runs fine.

- [ ] `httpx` and `papaparse` jslib imports are **stripped with no binding** → `ReferenceError` every iteration, rather than a clear unsupported-import error at load
- [ ] lodash shim: **35 common functions absent** and `_.sortBy(arr)` throws ✅**EXEC** — missing `groupBy, keyBy, orderBy, …`. Plus divergences over 193 executed cases: `_.padStart('7',4,'0')` → `'07'` (never repeats the pad string), `_.template` returns `''`
- [ ] Non-configurable **10 MB heap / 10 s deadline** (`driver.rs:69`, hardcoded at `:134,3037,3106,3184`) — a legitimate large-response script cannot run
- [ ] `handleSummary` return contract: `{destination: string}` where destination is `stdout`/`stderr`/a file path, and a **falsy return regenerates the default text summary**

---

# Track F — CLI and REST API

## TR-250 · REST API shape
**Effort:** M · **Blocked by:** none

- [ ] **[SILENT]** k6 keeps `api/v1` with **status, metrics, groups, setup, teardown** routes in JSON:API envelopes. Tropel uses `max` instead of **`vus-max`**, only accepts `POST /v1/stop` where k6 clients send `PATCH {"data":{"attributes":{"stopped":true}}}`, hardcodes `tainted` to null, and has **no `/v1/metrics`, `/v1/groups`, `/v1/setup`, `/v1/teardown`**
- [ ] **[SUPERSET]** k6 v2 turned the REST API **off by default** (`GlobalFlags.Address` → `""`). Tropel serving it by default is a deliberate divergence — **decide it consciously and document it**, especially given the unbounded header read in `TR-604`

## TR-251 · CLI surface
**Effort:** M · **Blocked by:** none

- [ ] **[GAP]** `--http-debug` / `--http-debug=full` request/response dumping with a per-request UUID
- [ ] **[GAP]** Subcommands tropel lacks: `k6 new`, `k6 stats`, `k6 report`, `k6 deps`, `k6 features`. (`run`/`inspect`/`archive` exist. `k6 cloud` and `k6 login` are out of scope — `login` was deleted upstream)
- [ ] `-o/--output` is silently ignored for `-r json` / `-r csv` ✅closed — keep the regression test
- [ ] Add `--no-thresholds`; no equivalent exists

---

# Track G — Postman parity

Tropel's `pm.*` runtime is a genuine competitive asset — **Bruno has no `pm` runtime at all**, and knockport's migration pitch rests on this. These are the defects that undercut it.

Full register: `TROPEL_PARITY_POSTMAN.md`.

## TR-260 · Path-variable substitution is a naive ordered `str::replace`
**Effort:** S · **Blocked by:** none

- [ ] `parser.rs:361-371` — `/users/:user/posts/:userId` substitutes `:user` inside `:userId`
- [ ] Descending-length sort works only when **both** tokens are present; the general fix is a single tokenising pass
- [ ] Same bug class as knockport `KP-425`'s importer — fix it here, since import parsing is Rust-side by decision D4

## TR-261 · Duplicate headers and form fields collapse into `HashMap`s
**Effort:** M · **Blocked by:** none

- [ ] `parser.rs:205-210` and `:398-431`. Two `Cookie:` headers become one; repeated form keys are lost
- [ ] Ordered multi-maps on both paths, matching Postman's own model

## TR-262 · `pm.*` runtime defects
**Effort:** M · **Blocked by:** TR-114

- [ ] `pm.request` in test scripts shows unresolved `{{templates}}` — `runner.rs:290` is never refreshed with the resolved request
- [ ] `pm.sendRequest` headers-array form is broken on the `pm` path; the k6 path was already fixed — the sibling-miss shape again
- [ ] `pm.request.body.mode` leaks the previous iteration's value via the module-scope fallback
- [ ] `bru.*` and `pm.*` use **two encoders on one variable store** ✅**EXEC**: `bru.setEnvVar('id','1234')` then `getEnvVar` returns the **number** `1234`; `bru.setVar('o',{a:1})` stores `"[object Object]"`
- [ ] `local_vars` is never cleared — Newman scopes `pm.variables` per request; here it grows for the whole run
- [ ] `__tropel_pm_test_skip` is registered by `TrpBridge`, but the k6 driver never installs `TrpBridge`

## TR-263 · Arbitrary local-file read driven by collection content
**Effort:** S · **Blocked by:** none · **Human sign-off**

- [ ] `tropel-collection/src/parser.rs:550` does `std::fs::read` on a path taken from the collection, and `:422,455` do the same for file bodies
- [ ] Expected for a self-authored collection; **not** acceptable for a collection imported from a URL or a shared repo — and knockport imports untrusted collections by design
- [ ] Confine reads to the collection root, require explicit opt-in for anything outside it, and report a refusal rather than silently sending an empty body
