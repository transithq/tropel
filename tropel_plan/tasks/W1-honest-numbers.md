# W1 · Honest numbers

**Gate:** no path where a broken thing reports success, and none where a working thing reports failure. This is the `0.1.0` release gate, and it is the whole reason the project is credible.

Two asymmetric failure classes, and the duplicate implementations that keep regenerating both. Source: `TROPEL_MASTER_TODO.md` §W1-A/B/C, §W2, §W-R4.

---

# Track A — Always-green: a broken thing reports success

The worst class. A user ships on a number that was never true.

## TR-101 · Capped series and `totals` disagree, and thresholds read the wrong one
**Effort:** M · **Blocked by:** TR-004

- [ ] `build_results` repairs `http_reqs`/`errors` from `totals`, but thresholds evaluate the **capped** series — so past `MAX_SERIES` the printed number and the evaluated number diverge
- [ ] `errors` and `errors.count` read different populations (`thresholds.rs:847-868`)
- [ ] `absorb_snapshot` bypasses the cardinality cap entirely — the `Vacant` arm inserts unconditionally (`collector.rs:1332-1343`)
- [ ] One population, one number. A test asserts the summary value and the threshold input are the same value past the cap

## TR-102 · A script can forge the `checks` headline
**Effort:** S · **Blocked by:** none

- [ ] The k6 driver has **no reserved-name guard** (`driver.rs:2276-23xx`), so user code can emit into the builtin `checks` Rate and make a CI gate read whatever it likes
- [x] Reserved builtin metric names reject user emission with a named error
- [ ] Same guard on the declarative path and the wasm driver — this is the "guard in one place, not its siblings" shape

## TR-103 · Both proxy guards leak `Object.prototype`
**Effort:** S · **Blocked by:** none

- [ ] `pm.js:566` and `chai-shim.js:715` use `prop in t`, so inherited properties resolve ✅**EXEC**
- [ ] `_obj`/`__flags`/`_actual` still resolve through both guards as own props of the target
- [ ] Use `Object.prototype.hasOwnProperty.call`, or a null-prototype target

## TR-104 · `pm.execution.stopOnError()` is a wired no-op
**Effort:** S · **Blocked by:** none

- [ ] `pm.js:1327` → `trp.rs:1050` sets `st.skip_tests`; `runner.rs:29x` never reads it — the canonical "a flag is set but nothing reads it" instance
- [ ] Wire it, and audit the three siblings of the same shape: SIGINT across the four duration executors, `tropel-web` force-stop, and the abort coordinator's unreachable `check_abort_on_fail`
- [ ] A test asserts the run stops at the failing request, not at the end

## TR-105 · stdout prints "✓ PASS" on runs that exit 1
**Effort:** S · **Blocked by:** none

- [ ] `stdout.rs:294-299` derives the banner from a different source than the exit code
- [ ] `summary.rs` keys the top-level `thresholds` map **by expression**, so duplicate expressions erase failures
- [ ] One verdict, computed once, used by the banner, the summary, the reporters and the exit code
- [ ] A test asserts banner and exit code agree across pass, fail, and no-data runs

---

# Track B — Always-red: a working thing reports failure

Less dangerous, equally fatal to adoption — nobody keeps a tool that fails their green build.

## TR-110 · The printed `http_req_failed` is never the one thresholds evaluate
**Effort:** M · **Blocked by:** TR-101

- [ ] `metrics.http_req_failed` is read by stdout while thresholds evaluate a different series
- [ ] Converge them; assert equality in a test rather than by inspection

## TR-111 · A no-data clause fails the whole compound
**Effort:** S · **Blocked by:** TR-011

- [ ] `thresholds.rs:312` propagates `None` out of the entire expression, so one metric with no observations fails an unrelated compound threshold
- [ ] "No data" is a third verdict, rendered distinctly from FAIL, and configurable in how it affects the exit code
- [ ] This is the same root cause as `TR-011`; fixing either alone leaves the other reachable

## TR-112 · Tag-scoped `avg` is worst-of; unscoped `avg` is pooled
**Effort:** M · **Blocked by:** none

- [ ] `thresholds.rs:516` vs `:665`. 1000 requests @10 ms on `/a` plus 10 @2000 ms on `/b` gives two different "averages" depending on whether the threshold names a tag
- [ ] One aggregation path, dispatching on `self.metric_type` — this single change also closes `avg > max`, the `absorb_snapshot` type conflict, and the reserved-name collisions
- [ ] Add the missing `value`/`last` arm in `aggregate_series` — it closes the Trend vacuous pass, tag-scoped arbitrary-series selection, and `vus:['value>10']` at the same time

## TR-113 · `handleSummary` has no unscoped `http_req_duration`
**Effort:** S · **Blocked by:** TR-112

- [ ] `summary.rs:23-84` iterates `results.metrics`; the merged unscoped series never appears, so the most common `handleSummary` script in existence reads `undefined`
- [ ] k6's v2.1.0 default `data` shape is still the legacy `{root_group, options, state, metrics, setup_data}` — match it
- [ ] `root_group` is currently a hardcoded empty stub while `per_group` is fully populated

## TR-114 · `pm.response.to.have.jsonBody("key")` is a false failure
**Effort:** S · **Blocked by:** none

- [ ] `pm.js:464` deep-equals the argument against the whole body instead of treating a string argument as a path assertion
- [ ] `to.have.status()` rejects the reason-phrase form in both `pm.js:437-445` and `chai-shim.js:714`
- [ ] Both fixes land with the six stock-snippet regression tests

---

# Track C — Aggregation correctness

## TR-120 · No NaN/Inf guard on the primary path
**Effort:** S · **Blocked by:** none

- [ ] The guard exists in the wasm driver and at two emitters, and **not** in `MetricSet::record` — the canonical sibling-miss
- [ ] A single NaN/Inf sample poisons a whole flush window across three outputs (`influxdb.rs:232`, `statsd.rs`, Prometheus)
- [ ] Guard once, at the point every path funnels through; delete the two partial guards
- [ ] A test feeds NaN through each of the four entry points and asserts the window survives

## TR-121 · Transport failures emit no `http_req_duration`
**Effort:** M · **Blocked by:** none · **Also k6 parity — see TR-203**

- [ ] `driver.rs:1057-1082` emits only `http_reqs` + `http_req_failed`; the declarative runner also omits `data_sent` (`runner.rs:715-7xx`)
- [ ] k6 emits **all 8** HTTP metrics on transport failure, with genuine zeros — dropping them makes `http_reqs` and `http_req_duration` counts disagree and **silently biases every percentile**
- [ ] Fix once, in the shared path, so both drivers inherit it

## TR-122 · `merged_percentile` never merges in production
**Effort:** M · **Blocked by:** TR-002

- [ ] `stat_needs_histogram` gates it such that the production path never reaches the merge, and **both tests that assert it are blind**
- [ ] Invert both tests in the same PR
- [ ] `retain_histograms` clones every Trend histogram on every 2 s tick (`collector.rs:964` and five siblings) — fix while you are in here, with a benchmark
- [ ] `merge_from` clobbers `last` with an empty series' zero (`collector.rs:281`, no `count > 0` guard)

---

# Track D — Collapse the duplicate implementations

Every item here is a wrong number **caused by** having two implementations. Fixing the number without collapsing the duplication just schedules the regression.

## TR-130 · Shared tag stringification on both HTTP paths
**Effort:** S · **Blocked by:** none

- [ ] `stringify_tag_map_into` (`driver.rs:877`) exists for exactly this and is used on one path
- [ ] Closes: `check()` tags, custom-metric tags, the batch whole-map drop, and the single-path filter — four items, one change

## TR-131 · `tropel-web/bootstrap.rs` calls the engine path
**Effort:** M · **Blocked by:** none

- [ ] Four divergences from `js_bootstrap.rs::create_vu_js_context`, plus a silent-success reporting path
- [ ] `bootstrap.rs` stops constructing its own context and calls the engine's
- [ ] The conformance suite runs against the web slice too, or this reappears

## TR-132 · One shim list, one `Method` parser, one deep-equal
**Effort:** M · **Blocked by:** TR-013

- [ ] Two `Method` parsers in one file: `trp.rs:94-111` (empty → GET, no tchar validation, uppercases `Custom`) vs `types.rs` — keep the strict one
- [ ] `ShimBundle::render()` is not byte-identical to `JS_SHIM_BUNDLE` (one trailing `\n`), contradicting the comment that claims it is
- [ ] Three near-identical deep-equal copies collapse to the one fixed in `TR-013`

## TR-133 · Group-path tagging on every path
**Effort:** S · **Blocked by:** none

- [ ] `::a::b` is correct for http/checks/group_duration; `ws_*` hardcodes the group
- [ ] Group-tag semantics diverge between drivers — the k6 driver stamps the full `::a::b`, the sandbox stamps the leaf
- [ ] k6's rules: root is `""`, a leading `::` always, and a name containing `::` is a **hard error**

## TR-134 · The npm workspace resolves `@tropel/shims` locally
**Effort:** S · **Blocked by:** TR-008

- [ ] With no root `package.json` workspace entry and no `file:` dep, `@tropel/shims` resolves **from npmjs.org** — the published copy, not the tree you are editing
- [ ] Every package in `packages/` is a workspace member (asserted by the `TR-008` check)

## TR-135 · Sweep the seven lying comments and the four bug-pinning tests
**Effort:** S · **Blocked by:** none

- [ ] Seven comments describe the intended fix rather than shipped behaviour: Digest cache, refcount-1, "k6's exact schema", case-insensitive Cookie, `$ref` bounded, byte-identical bundle, bru store aliasing
- [ ] The four bug-pinning tests are listed in `CONVENTIONS.md`; delete or invert each as its fix lands, and remove the entry from the table
- [ ] `types.rs:773` and `types.rs:910` each hold a literal NUL byte — one hides a self-contradictory test that renders as `" GET"`, the other makes `grep`/`rg` treat the file as binary
