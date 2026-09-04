# `__tropel_trp_*` — the shim/host bridge contract

**Status:** normative. If an implementation and this file disagree, that is a bug in one of them — say which.

## What this is

`@tropel/shims` is JavaScript. It calls out to the host for everything stateful: the request being
built, the response that came back, the variable scopes, test results. Those calls are the
`__tropel_trp_*` globals, and **every host must provide all of them.**

There are two hosts today:

| Host | Implementation | JS engine |
|---|---|---|
| tropel runtime | `crates/tropel-sandbox/src/bindings/trp.rs` (Rust) | QuickJS (`rquickjs`) |
| KnockPort | `packages/engine/src/scripting-core.ts` (TypeScript) | the browser's / node's |

## Why the contract is written down

The bridge is glue between a JS engine and host state. **A browser host cannot reuse `trp.rs`** —
that file registers `rquickjs::Func` values into a QuickJS context, so reusing it means shipping
QuickJS (~1.5 MB) to a runtime that already has a JavaScript engine. So the second implementation
is unavoidable.

Two implementations is fine. Two **unspecified** implementations is a fork.

Browsers implement JavaScript independently and nobody calls that a fork, because test262 exists.
This file is the spec half of the same idea; the conformance corpus is the test half.

The cost of not having it is already on record: a TypeScript re-implementation of the *variable
resolver* drifted in two ways — a grammar that could not match a hyphen, and one escape mode where
there are three — and neither failed anything. Wrong bytes went on the wire silently for months.

## Rules a host must follow

1. **Every function must exist.** A missing global is a `ReferenceError` inside shim code, which
   surfaces as an incomprehensible script failure rather than a named refusal.
2. **An unsupported operation THROWS, with a reason.** Never a silent no-op, and never a plausible
   default. A host that cannot mutate the request URL must throw naming why — returning `undefined`
   makes `pm.request.url = x` look like it worked.
3. **Arguments are not optional.** A host that accepts a call and discards its arguments is the
   no-op case in rule 2 wearing a disguise. (Real example: `group_start`/`group_end` were
   `() => {}` in the TypeScript host — accepted, discarded, returned.)
4. **`Option<String>` means absent, not empty.** Rust returns `None`; a JS host must return
   `undefined` or `null`, NOT `""`. The shims distinguish them.
5. **`to_object` returns a plain string→string map.** Values are stringified by the host.
6. **Nothing here may block indefinitely.** The shims are synchronous by construction (a QuickJS
   embedding constraint); a host that needs async work must replace the *binding*, not stall the
   bridge — see the note on `send_request` below.

## The signatures

Extracted from `trp.rs`, which is the reference implementation. Rust types; the JS host equivalents
follow rule 4 and rule 5.

### Context

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_info` | `—` | `String` |

### Variables · runtime (pm.variables)

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_variables_get` | `key: String` | `Option<String>` |
| `__tropel_trp_variables_set` | `key: String, value: String` | `()` |
| `__tropel_trp_variables_unset` | `key: String` | `()` |

### Variables · environment

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_environment_clear` | `—` | `()` |
| `__tropel_trp_environment_get` | `key: String` | `Option<String>` |
| `__tropel_trp_environment_has` | `key: String` | `bool` |
| `__tropel_trp_environment_set` | `key: String, value: String` | `()` |
| `__tropel_trp_environment_to_object` | `—` | `HashMap<String, String>` |
| `__tropel_trp_environment_unset` | `key: String` | `()` |

### Variables · collection

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_collection_vars_get` | `key: String` | `Option<String>` |
| `__tropel_trp_collection_vars_has` | `key: String` | `bool` |
| `__tropel_trp_collection_vars_set` | `key: String, value: String` | `()` |
| `__tropel_trp_collection_vars_to_object` | `—` | `HashMap<String, String>` |
| `__tropel_trp_collection_vars_unset` | `key: String` | `()` |

### Variables · globals

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_globals_get` | `key: String` | `Option<String>` |
| `__tropel_trp_globals_has` | `key: String` | `bool` |
| `__tropel_trp_globals_set` | `key: String, value: String` | `()` |
| `__tropel_trp_globals_to_object` | `—` | `HashMap<String, String>` |
| `__tropel_trp_globals_unset` | `key: String` | `()` |

### Variables · iteration data

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_iteration_data_get` | `key: String` | `Option<String>` |

### Request

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_request_auth` | `—` | `Option<String>` |
| `__tropel_trp_request_auth_set` | `auth_json: String` | `()` |
| `__tropel_trp_request_body` | `—` | `Option<String>` |
| `__tropel_trp_request_body_mode` | `—` | `String` |
| `__tropel_trp_request_body_set` | `body: String` | `()` |
| `__tropel_trp_request_header_get` | `key: String` | `Option<String>` |
| `__tropel_trp_request_header_set` | `key: String, value: String` | `()` |
| `__tropel_trp_request_header_unset` | `key: String` | `()` |
| `__tropel_trp_request_headers` | `—` | `HashMap<String, String>` |
| `__tropel_trp_request_method` | `—` | `String` |
| `__tropel_trp_request_method_set` | `method: String` | `()` |
| `__tropel_trp_request_url` | `—` | `String` |
| `__tropel_trp_request_url_set` | `url: String` | `()` |

### Response

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_response_body` | `—` | `Option<String>` |
| `__tropel_trp_response_code` | `—` | `u16` |
| `__tropel_trp_response_cookies` | `—` | `HashMap<String, String>` |
| `__tropel_trp_response_header` | `key: String` | `Option<String>` |
| `__tropel_trp_response_headers` | `—` | `HashMap<String, String>` |
| `__tropel_trp_response_json` | `—` | `Option<String>` |
| `__tropel_trp_response_status` | `—` | `String` |
| `__tropel_trp_response_time` | `—` | `f64` |

### Tests

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_test_skip` | `name: String` | `()` |

### Flow control

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_set_next_request` | `request_id: Option<String>` | `()` |
| `__tropel_trp_skip_request` | `—` | `()` |

### Side calls

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_send_request` | `method: String, url: String, headers_json: String, body: String, timeout_ms: f64, response_type: String` | `String` |

### Metrics

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_custom_metric_add` | `name: String, value: f64, tags_json: String, metric_type_str: String` | `()` |
| `__tropel_trp_metrics_add` | `name: String, value: f64, metric_type_str: String` | `()` |
| `__tropel_trp_metrics_get` | `name: String` | `Option<f64>` |

### Grouping

| Function | Args | Returns |
|---|---|---|
| `__tropel_trp_group_end` | `name: String, duration_ms: f64` | `()` |
| `__tropel_trp_group_start` | `name: String` | `()` |
## Notes on the awkward ones

### `__tropel_trp_send_request`
Synchronous by signature — it returns the response as a JSON string. That works in a QuickJS
embedding where the host can block; it does **not** work in a browser, where the transport round
trip is a Promise.

KnockPort therefore does **not** implement this bridge. It replaces the *binding* instead:
`pm.sendRequest` / `kp.sendRequest` are overwritten in the realm prelude with an async
implementation that rides the pipeline's transport seam. That is the sanctioned pattern for
anything the sync contract cannot express — **replace the binding, do not fake the bridge.** A host
doing this must say so, as the absence otherwise reads as a gap.

### `__tropel_trp_test` and `__tropel_trp_variables_to_object`
Present in the TypeScript host, absent from `trp.rs`. `test` records a result the Rust side takes
through a different path; `variables_to_object` mirrors the three `*_to_object` siblings. Both are
**additive** — a host may provide more than the contract, never less — but they are listed here so
the asymmetry is a decision rather than a discovery.

### Load-run-only surface
`metrics_*`, `custom_metric_add`, `group_*` and `iteration_data_get` only mean something inside a
run with VUs and iterations. A single-request host should **refuse them by name** (rule 2) rather
than no-op, so a script that reaches for them says why nothing happened.

## Keeping this file true

It is generated from `trp.rs`'s registrations. When a bridge is added, changed or removed there,
update this table in the same commit — a contract that lags its reference implementation is worse
than no contract, because it is believed.
