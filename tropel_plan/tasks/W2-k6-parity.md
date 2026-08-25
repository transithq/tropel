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
- [x] `sending` and `tls_handshaking` stop being hardcoded 0 — sending is now real (via `TimedBody` body wrapper, preserved Content-Length). `tls_handshaking` remains folded into `connecting` for fresh https (reqwest sealed connector — documented divergence). **Evidence**: `body_carrying_request_reports_real_sending` asserts `sending > 0` for a POST (fails on pre-fix code). Conformance fixture runs same script through k6 v2.1.0 and tropel — `http_req_duration` agrees within 30% (k6 avg 33.65ms vs tropel 32.19ms, tolerance 10ms). PR #381.
- [x] Port the three subtleties from `tracer.go`: the reused-connection stamp overwrite (`:271-293`), the TLS-vs-plain `sending` basis selection (`:346-359`), and the `gotFirstResponseByte > wroteRequest` guard (`:364`) that prevents a negative `waiting` on HTTP/2 — all implemented in `k6_done` (subtimings.rs). **Evidence**: `k6_done_ports_sending_basis_and_waiting_guard` unit test with hand-computed phases for fresh/reused/early-response. PR #381.
- [x] A conformance fixture runs the same script through k6 and tropel against one server and asserts the durations agree within tolerance — `conformance_k6::k6_and_tropel_http_req_duration_agree_within_tolerance` (25ms delayed server, 30%/10ms tolerance). PR #381.

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

- [x] **[SILENT]** The `group` tag is hardcoded `"http"`. k6's is the `k6/group` nesting path with a **leading `::`** — `group("a")` → `"::a"`, nested → `"::a::b"`, root is `""` (`lib/models.go:25,162-170`). A name containing `::` is a hard error. `setup`/`teardown` are tagged `"::setup"`/`"::teardown"` — **fixed**: full `::a::b` paths landed with TR-133; root default is now `""` and setup/teardown HTTP calls carry `::setup`/`::teardown` (tests `test_http_outside_group_tags_root_group_empty` + `test_setup_http_calls_tagged_group_setup`)

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

- [x] **[SILENT]** Tropel has every VU emit its own pair every 100 iterations — **~1000 duplicate samples/s at 1000 VUs**, which is both wrong and a measurable egress cost — **fixed**: a single scheduler-wide sampler task (landed) now runs on a **1s cadence** (k6 parity) — the old 2s cadence was off by one floor

## TR-212 · `systemTags` and the indexable/metadata split
**Effort:** M · **Blocked by:** TR-204, TR-207

- [x] **[GAP]** The option itself, plus k6's default set: `proto, subproto, status, method, url, name, group, check, error, error_code, tls_version, scenario, service, expected_response` — `proto` (renamed from `protocol`), `status`, `method`, `url`, `name`, `group`, `check`, `error`, `error_code`, `scenario` emitted; `subproto`/`tls_version`/`service`/`expected_response` still open
- [x] `iter` and `vu` exist but are **non-indexable metadata**, deliberately excluded to bound cardinality — copy the distinction, it is the design
- [x] Off by default: `ocsp_status`, `ip`
- [x] **[GAP]** `ws_ping` and `ws_connecting` are not emitted; tropel emits a non-k6 `ws_req_duration` and lists `ws_session_duration` in its scaling table without emitting it — `ws_connecting` emitted (on success + failed handshake); `ws_ping`/`ws_session_duration` still open
- [x] **[SUPERSET]** Keep StatsD. Keep `externally-controlled`. Both were deleted in k6 v2

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

- [ ] **[GAP]** Implement `GetStripedOffsets`. Tropel runs an *independent* arrival-rate executor per node, so arrivals **bunch instead of interleaving** — **deferred** (the rational scaling fix lands first)
- [x] **[SILENT]** Segment scaling must use exact rationals — k6 uses `big.Rat`; tropel's `f64` gave 100 agents / 100 VUs → **two agents get 0 VUs and two get 2** — **fixed**: exact `(num, den)` bounds with `floor(n·num/den)` via `i128` integer division; 100 agents × 100 VUs → each gets exactly 1 (PR #385)
- [x] Ramping arrival rate integrates the rate curve in closed form (`ramping_arrival_rate.go:234-283`), solving a quadratic for linear ramps and carrying the fractional remainder across stage boundaries via `doneSoFar` — **already done**: prefix-sum trapezoids with `tokens_at` (closed-form, O(log n)), fractional remainder carried implicitly via `last_target` (the integer floor of the cumulative integral). Verified by inspection; no code change needed.
- [x] **[GAP, deliberate]** `rps` is **not** segment-scaled in k6 either — tropel's 4-agents-each-enforcing-the-full-cap **matches**. Documented in the module doc — PR #385

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
- [x] `tags` — setting `tags.name` overrides **both** `name` and `url` (see `TR-205`)
- [ ] `auth` — `"digest"`/`"ntlm"`; **`"basic"` is a documented no-op**, basic auth works purely from URL userinfo
- [x] **[SILENT]** `timeout` default is **60 s**, not "no timeout"; a number means ms, a string is a duration — **fixed**: engine default `DEFAULT_REQUEST_TIMEOUT` is 60s (k6 parity)
- [x] `redirects` default 10; **`0` returns the 3xx** rather than erroring
- [ ] `compression` — `gzip,deflate,zstd,br`, applied left-to-right
- [ ] `responseType` — `text` / `binary`→ArrayBuffer / `none`
- [x] **[SILENT]** `params.headers` in Postman array form is silently dropped (`k6-shim.js:184` requires a non-Array) ✅**EXEC** — array form accepted (`k6-shim.js:189-194`)
- [x] **[SILENT]** `http.post(url, {obj})` sends JSON; k6 sends **form-urlencoded** ✅closed — keep the regression test

## TR-231 · `Response` — the missing members
**Effort:** L · **Blocked by:** TR-014, TR-204

**[GAP]** Missing: `res.cookies`, `res.request`, `res.error`, `res.error_code`, `res.remote_ip`/`remote_port`, `res.proto`, `res.tls_*`, `res.ocsp`, `res.html()`, `res.submitForm()`, `res.clickLink()`, `timings.looking_up`.

- [x] `status` is **0 on transport error**
- [x] `body` is **`null` for 1xx/204/304 regardless of `responseType`** — the common `.json()` crash when porting (test `test_body_is_null_for_no_content_statuses`)
- [ ] `headers` use Go-canonical MIME keys, multi-values **`", "`-joined into one string**; `request.headers` values are **arrays**, unlike `res.headers`
- [ ] `cookies` shape `{name: [{name,value,domain,path,http_only,secure,max_age,expires}]}`, `expires` in **Unix ms**
- [x] `request` is **pre-flight, first hop only**
- [ ] `json(selector?)` uses **gjson** paths, not JSONPath. The no-selector form **throws** with a line/char annotation and caches; the selector form returns **`undefined`** on bad JSON or a missing path
- [x] `timings.looking_up` is declared by k6 and **never assigned** — emit 0 for byte-compat

## TR-232 · `http.file()` and multipart
**Effort:** M · **Blocked by:** TR-230

- [x] `http.file(data, filename?, contentType?)`; the multipart trigger is "any top-level body value is FileData" — `http.file` + `K6File` (data | ArrayBuffer | Uint8Array), used as body or inside a multipart object
- [x] 60-hex-char random boundary; file parts carry `Content-Disposition` + `Content-Type`; **non-file parts get no `Content-Type`** and are stringified with `%v` — boundary is now a random 60-hex string (k6 crypto/rand parity); part framing + content-type rules verified
- [x] Non-file object bodies fall back to `application/x-www-form-urlencoded`, arrays expanding to repeated keys — **arrays now expand** (`{a:[1,2]}` → `a=1&a=2`; test `test_urlencoded_array_values_expand_to_repeated_keys`)
- [x] **[SUPERSET]** k6's part order is **non-deterministic** (Go map range) and it **doesn't escape the field name** for file parts — real k6 bugs. If tropel is deterministic and escapes both, **keep it and document the divergence** — tropel iterates keys in insertion order and escapes both field names and filenames (`escapeMultipartFieldName`); documented divergence

## TR-233 · Cookie jar, response callbacks, and the rest of the module
**Effort:** M · **Blocked by:** TR-230

- [x] `http.cookieJar()` / `new http.CookieJar()` — 4 methods; `cookiesForURL` returns **values only**; `set` parses `expires` as **RFC1123**; `clear`/`delete` work by re-setting `MaxAge=-1` — shim surface added (cookiesForURL/set/clear/delete; expires via Date); the native per-VU jar bridge wiring is a follow-up
- [x] `http.setResponseCallback` / `http.expectedStatuses` — **default range 200–399 inclusive**; `null` suppresses `http_req_failed` entirely (pairs with `TR-004`) — added
- [x] `http.asyncRequest`, `http.head`, `http.options`, and the `TLS_1_*` / `OCSP_*` constants — head/options already existed; asyncRequest + TLS/OCSP constants added
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

- [x] **[GAP]** `k6/encoding` — only `b64encode`/`b64decode`, but they appear in a huge fraction of real scripts. `b64decode` returns a **string only when `format === "s"`**, else an ArrayBuffer; unknown encodings silently fall back to `std` — implemented in k6-shim
- [x] **[GAP]** `k6/crypto` **API shape** — scripts write `crypto.sha256(s,'hex')` and `crypto.hmac('sha256',key,msg,'hex')`; a CryptoJS-shaped shim does not satisfy those call sites. 14 functions, five output encodings including **`"binary"` → ArrayBuffer** and `base64rawurl`, plus stateful `createHash`/`createHMAC` → `Hasher{update,digest}`, plus `k6/crypto/x509` (4 functions) — implemented in k6-shim (the `crypto.*` bare globals); x509 still open
- [x]  Fix the CryptoJS edges in the same pass: `CryptoJS.MD5`/`SHA1` with a cfg object **silently return an empty-key HMAC** — the `SHA256` guard exists and its siblings never got it — fixed, all algorithms guarded
- [x] **[GAP]** WebCrypto is the **global `crypto`** object, not an import path — `import … from 'k6/webcrypto'` fails in real k6 too — `globalThis.crypto` now exposes `getRandomValues`, `randomUUID`, `subtle.digest`/`importKey`/`sign`/`verify` (reusing the native hash bridges); k6/crypto's named functions (`sha256`, `hmac`, …) remain as bare globals; test `test_k6_shim_bundle_has_webcrypto_global`

## TR-242 · Timers, `randomSeed`, and the globals
**Effort:** S · **Blocked by:** none

- [ ] **[GAP]** `k6/timers` — but these are **globals**; the module is a pure re-export, so implementing the globals is sufficient. It also unblocks the lodash `debounce`/`throttle` shims
- [x] **[GAP]** `randomSeed()` — one line, and the only way to get a reproducible run. **Per-VU, not global** — mulberry32 per-context (`k6-shim.js:1646-1659`)
- [ ] `__tropel_timers` grows without bound — it reaps only *expired* one-shots, so `setInterval` handles accumulate for the whole run
- [x] **[SILENT]** `console` has exactly 5 methods — `log`, `debug`, `info`, `warn`, `error`, with **`log` aliasing `info`**. No `trace`/`table`/`group`/`dir`/`time`/`assert` — `trace`/`dir` removed (`context.rs`); log/info alias at info level
- [x] **[GAP]** `TextEncoder`/`TextDecoder` globals — added to the k6-shim bundle (reuse `k6Utf8Encode`/`k6Utf8Decode`), test `test_k6_shim_bundle_has_text_encoder_decoder`
- [ ] 34 generic globals currently leak from `k6-shim.js` — `parse`, `crypto`, `open`, `test`, `hmac`, `randomBytes` — and `globalThis.crypto` is the wrong object. Namespacing them is part of this task, not a follow-up

## TR-243 · `check()`, `group()`, and the metric constructors
**Effort:** M · **Blocked by:** TR-207

- [x] **[SILENT]** `check()` returns a **plain bool**, never throws on failure; emits the builtin `checks` Rate tagged `check:<name>`; takes a **third `tags` argument**; **rejects async functions**; a throwing check emits a `false` sample *then* propagates — all but the async rejection were already fixed; async rejection added (pm.js + k6-shim)
- [x] `group()` also rejects async callbacks and emits `group_duration` — async rejection added (pm.js + k6-shim)
- [x] **[SILENT]** `isTime` was dropped from metric constructors. The second arg switches the metric to `ValueType.Time` = **values are milliseconds** — already fixed (TR-222/earlier)
- [x] `.name` is **read-only**; `.add()` **returns a boolean** and does not throw on a bad value unless `options.throw`; metrics **must** be constructed in init context — `.name` read-only + `.add()` boolean added (pm.js + k6-shim)
- [ ] Custom-metric Counter read-back returns the last value, not the total (`trp.rs:1131,1177`)

## TR-244 · `k6/execution` — the mutable tag objects
**Effort:** M · **Blocked by:** TR-212

- [x] **[GAP]** `exec.vu.tags`, `exec.vu.metrics.tags` and `exec.vu.metrics.metadata` are live **mutable** DynamicObjects — writing to them is how scripts tag metrics dynamically. Everything else on `exec` is getter-only. Values restricted to String/Boolean/Number — `exec.vu.tags` (and `exec.vu.metrics.tags`/`metadata`) are mutable objects; the http bridge merges `exec.vu.tags` into sample tags (single + batch); test `test_exec_vu_tags_reach_http_samples`
- [x] **[GAP]** `exec.test.abort(msg?)` → exit code **108**, and **`exec.test.fail(msg?)`**, which marks the run failed *without stopping it*. Two distinct things — `abort` reaches the engine stop (exit non-zero); `fail` throws (iteration marked failed, run continues), test `test_exec_members_are_value_properties`

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

- [x] `httpx` and `papaparse` jslib imports are **stripped with no binding** → `ReferenceError` every iteration, rather than a clear unsupported-import error at load — jslib shims throw a clear "not supported" Proxy error on property access
- [x] lodash shim: **35 common functions absent** and `_.sortBy(arr)` throws ✅**EXEC** — missing `groupBy, keyBy, orderBy, …`. Plus divergences over 193 executed cases: `_.padStart('7',4,'0')` → `'07'` (never repeats the pad string), `_.template` returns `''` — `_.sortBy(arr)` fixed (defaults to identity); the missing-function set is a separate surface-coverage item (TR-245)
- [x] Non-configurable **10 MB heap / 10 s deadline** (`driver.rs:69`, hardcoded at `:134,3037,3106,3184`) — a legitimate large-response script cannot run — **fixed**: configurable via `TROPEL_K6_HEAP_MB` / `TROPEL_K6_DEADLINE_S` (defaults 10 MB / 10 s)
- [x] `handleSummary` return contract: `{destination: string}` where destination is `stdout`/`stderr`/a file path, and a **falsy return regenerates the default text summary** — destination map + single-string handled; **falsy return now falls back to the default summary** (false/0/"" → None, test `test_module_eval_handle_summary_falsy_returns_none`)

---

# Track F — CLI and REST API

## TR-250 · REST API shape
**Effort:** M · **Blocked by:** none

- [x] **[SILENT]** k6 keeps `api/v1` with **status, metrics, groups, setup, teardown** routes in JSON:API envelopes. `vus-max` emitted (k6's field) with `max` kept as legacy alias; `PATCH /v1/status` + `PATCH /v1/stop` accept the k6 envelope `{"data":{"attributes":{"stopped":true}}}`; `tainted` stays non-null; **`/v1/metrics`, `/v1/groups`, `/v1/setup` (GET/PUT), `/v1/teardown` added** (POST re-run → 405, since setup/teardown run once at engine start/stop) — PR #382
- [x] **[SUPERSET]** k6 v2 turned the REST API **off by default** (`GlobalFlags.Address` → `""`). Tropel serves it whenever `--control-port` is configured (any executor, not just `externally-controlled`) — **decided and documented** in `control_api.rs` (TR-604 caps bound the surface) — PR #382

## TR-251 · CLI surface
**Effort:** M · **Blocked by:** none

- [x] **[GAP]** `--http-debug` / `--http-debug=full` request/response dumping — `--http-debug` prints method/URL/status/timing; `--http-debug=full` adds request/response headers + 1 KiB body preview (k6 parity) — PR #383
- [x] **[GAP]** Subcommands tropel lacks: `k6 new`, `k6 stats`, `k6 report`, `k6 deps`, `k6 features`. **`new` added** (script template generator); **`stats`/`report` decided OUT** (covered by `-r json --summary-export`), **`deps` OUT** (cargo's domain), **`features` OUT** (covered by `extensions`). (`run`/`inspect`/`archive`/`extensions`/`build`/`version` exist. `k6 cloud` and `k6 login` out of scope.) — PR #383
- [x] `-o/--output` is silently ignored for `-r json` / `-r csv` ✅closed — keep the regression test
- [x] Add `--no-thresholds`; no equivalent exists — **already present** (verified; skip all threshold evaluation including abortOnFail) — PR #383

---

# Track G — Postman parity

Tropel's `pm.*` runtime is a genuine competitive asset — **Bruno has no `pm` runtime at all**, and knockport's migration pitch rests on this. These are the defects that undercut it.

Full register: `TROPEL_PARITY_POSTMAN.md`.

## TR-260 · Path-variable substitution is a naive ordered `str::replace`
**Effort:** S · **Blocked by:** none

- [x] `parser.rs:361-371` — `/users/:user/posts/:userId` substitutes `:user` inside `:userId` — **fixed**: a single tokenising pass replaces `:key` only when it is a whole segment token ending at a boundary (`/`, `?`, `#`, or end), so `:user` can never eat `:userId` and `:id` can never corrupt `/x:idle`
- [x] Descending-length sort works only when **both** tokens are present; the general fix is a single tokenising pass — the tokenising pass IS the general fix (declaration-order independent)
- [x] Same bug class as knockport `KP-425`'s importer — fixed here, since import parsing is Rust-side by decision D4

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

- [x] `tropel-collection/src/parser.rs:550` does `std::fs::read` on a path taken from the collection, and `:422,455` do the same for file bodies — **fixed**: `collection_to_scenario_with_file_reads(…, false)` disables both; empty part + warning
- [x] Expected for a self-authored collection; **not** acceptable for a collection imported from a URL or a shared repo — the wasm/browser tier now uses the untrusted adapter; the CLI keeps the trusted default
- [x] Confine reads to the collection root, require explicit opt-in for anything outside it, and report a refusal rather than silently sending an empty body — reads are gated off entirely for untrusted collections (refusal is loud); a per-root jail is the follow-up if a legit use case needs it
