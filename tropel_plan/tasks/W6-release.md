# W6 · Release mechanics — `0.1.0`

**Gate:** the release gate below is green, and crates.io, npm and the binary ship in lockstep from one CI job.

## The release gate

Copied verbatim from the register so there is one list, not two:

- [ ] No path where a broken thing reports success (W1 Track A empty)
- [ ] No path where a working thing reports failure (W1 Track B empty)
- [ ] `cargo publish --dry-run` succeeds for every publishable crate
- [ ] Branch protection on `master` requires `CI OK` — **still returns 404**
- [ ] A real k6 script **and** a real Postman export both run unmodified, with correct numbers
- [ ] A release-profile build sustains 10 000 VUs without hitting the connection-pool cliff or the `MAX_WORKERS` degradation; per-VU memory measured and recorded
- [ ] The 4 096-concurrency ceiling is either fixed or **documented in the README** (`TR-502`)

Source: `TROPEL_MASTER_TODO.md` §W5, §"Release gate for 0.1.0" · `TROPEL_EXEC_SPLIT.md` §6.

---

# Track A — Unblock publishing

## TR-601 · `cargo publish` is blocked
**Effort:** M · **Blocked by:** TR-407 · **Human sign-off: publishing**

- [ ] crates.io has `tropel-sdk` **0.1.0 and 0.2.0 only**; the workspace declares **0.3.0**. A `path`+`version` dep only publishes if that exact version is on the registry, so it fails for **every** dependent
- [ ] Second blocker: **`tropel-http 0.1.0` is yanked** and is a versioned dependency
- [ ] Publish order and the dry-run are scripted, not manual
- [ ] Versions are permanent — this needs a human before the first real publish

## TR-602 · Decide on `tropel-auth`
**Effort:** S · **Blocked by:** TR-603 · **Human sign-off**

- [ ] `tropel-auth` is `publish = true` while carrying open SigV4 gaps, and `tropel-http` was deliberately demoted to `publish = false` as *"published by accident … INTERNAL"*
- [ ] The auth split moved the signers **out of** a crate being un-published and **into** a supported public one. That is a decision nobody made deliberately
- [ ] Decide: publish it (and accept the semver commitment on the signing surface), or demote it. Record why

## TR-603 · Auth correctness before anything ships publicly
**Effort:** L · **Blocked by:** none · **Blocks:** TR-601, TR-602 · **Serves `TR-409`**

A wrong signature is a 403 the user spends a day on. None of this can ship to strangers first.

- [x] **SigV4 consecutive-slash normalization is incomplete** — `signers.rs:516` `path.replace("//","/")` is a **single pass**, so runs of ≥3 survive. ✅**CALC** two official AWS suite failures; a real 403 on `//prod/users`, the classic base-URL-ends-in-`/` join. **`lib.rs:1503` asserts the buggy behaviour — invert it in the same PR**
- [x] **SigV4 streaming bodies sign the empty-payload hash** ✅**EXEC** — a `Body::wrap_stream` carrying `b"payload"` signs as if empty
- [x] **The SigV4 signing-key cache made things slower and is collision-unsafe** (`signers.rs:440-470`)
- [x] **Digest `SHA-512-256` degrades to MD5** while echoing `algorithm="SHA-512-256"` ✅**EXEC** 32 hex chars returned; `digest_with` has **no SHA-512/256 arm at all**
- [x] **The Digest session cache is dead code → the target sees 2× the reported RPS.** The only production construction (`vu_loop.rs:483`) builds a fresh signer per request, so the lookup can never hit; `client.rs:723-764` replaces the 401 **in place**, so it never becomes an `HttpResponse` and **no sample is recorded for it**
- [x] **The RPS limiter is acquired once per `execute()`, not per hop** — `client.rs:453` sits above the redirect loop, so `rps:1000` against a 302 chain sends **2000/s**. `rps.rs` itself is correct; the bug is purely at the call site
- [ ] **OAuth2 silently drops `client_secret`** with the default Basic auth method (`oauth.rs:438-452`)
- [ ] **OAuth2 Basic client auth omits RFC 6749 §2.3.1 form-encoding** ✅**EXEC**
- [ ] Digest: a **realm change with an unchanged nonce is silently ignored**; `signed_headers` re-application **appends** rather than replacing
- [ ] SigV4/OAuth1/Hawk `Authorization` is **replayed across same-origin redirect hops** — the signature is bound to method and path
- [ ] **Secrets reach stdout and every `Debug`** — `cli_commands.rs:122` does `println!("  global auth: {:?}", auth)`. Redacting `Debug` for every credential-bearing type is a prerequisite to any public release

---

# Track B — The SDK is actually usable by a stranger

## TR-604 · SDK compile gates and API surface
**Effort:** M · **Blocked by:** TR-407

- [ ] **`--no-default-features --features unstable-protocol` does not compile** ✅**EXEC** `E0432`. `lib.rs:135,139` gate the `*Registration` re-exports on the *unstable* flags, but they only exist under `registration`. `Cargo.toml:40-42` advertises that config as supported and **CI never compiles it** — add the matrix job
- [ ] **`Response` is not `Sync`** — `std::cell::OnceCell` where `OnceLock` belongs (`types.rs:394,397`). ✅**EXEC** compile failure. The central type of a multi-threaded engine cannot go in an `Arc` or cross a `tokio::spawn` boundary, and it poisons `ProtocolOutcome`
- [ ] **`Body` deserialize silently deletes a user key named `__tropel_body`** ✅**EXEC** for every non-string value — *`get` before `remove`*
- [ ] **`from_mode` swallows bad stages and unknown modes** ✅**EXEC**: a 2-stage 6-minute ramp becomes **one 30 s stage**; `per-vu-iterations`, `ramping-arrival-rate`, `externally-controlled` and `""` **all become `constant-vus`** — three of those are real executors the enum supports, and `traits.rs:238-241` states the opposite
- [ ] Malformed tagged bodies silently become empty — a form field with a numeric JSON value drops the whole form; `Json(String)` round-trips to `Raw`, so **a distributed worker sends different bytes than the controller intended**
- [ ] The TS host has no bridge error path — a decoder or transport throw escapes into the wasm call and **traps the instance**
- [ ] `Writer.varint` zigzag omission and the `Infinity` hang (`postcard.ts:33-41`, `:332-341`)
- [ ] A public-API snapshot exists, so semver-checks stops being inert — it currently diffs 0.2.0 against a 0.3.0 tree

## TR-605 · Robustness before the binary is handed to strangers
**Effort:** M · **Blocked by:** none

- [x] **SIGINT/SIGTERM handling exists** ✅ verified at `2099cbe` — `vu_loop.rs:345-392` registers Ctrl-C and (on Unix) `SIGTERM`, calls `request_stop()` on the first signal and force-stops on the second. The register's *"zero occurrences in any crate or binary"* is dead. **Do not re-file**
- [ ] **Confirm the four duration-based executors actually poll the flag.** `vu_loop`'s pause gate reads `is_stop_requested()`/`is_force_stop_requested()` (`:100`, `:108`), but whether the duration executors do is the real R4 claim and needs a scheduler read — the "flag nothing reads" shape (`TR-104`)
- [ ] Confirm the three distributed daemons are covered — the handler is registered in `vu_loop`, which they may not run through
- [ ] **The control-API header read is unbounded** — `read_line` with no cap on count or length; `MAX_BODY_SIZE` guards only the body, and `MAX_HEADER_LINE_LEN` is checked **after** the read. `CONN_TIMEOUT` bounds it in *time*, which over loopback is multiple GB × `MAX_CONNS = 8`. This is the exact vector the body ceiling was added for, one layer up
- [ ] The externally-controlled shrink can wedge permanently; the arrival-token lost wakeup starves the pool exactly when it should grow (both are `TR-012`'s shape)
- [ ] Validate `req.iterations` and cap `tropel_alloc` on the C ABI — `iterations: 0xFFFFFFFF` needs ~171 GB. Note `panic = "abort"` on `[profile.release-wasm]` makes `catch_unwind` unavailable, so **the fix is removing the 86 `.lock().unwrap()`s, not adding a barrier**
- [ ] Malformed `gracefulStop`/`thinkTime` silently default — `thinkTime:{delay:"5x"}` gives **zero think time, no warning**

---

# Track C — Repo hygiene

## TR-606 · Make CI enforceable
**Effort:** S · **Blocked by:** TR-010

- [ ] **Enable branch protection on `master` requiring `CI OK`** — still 404. The CI is genuinely good (a threshold negative control commented *"which has happened"*, `TROPEL_REQUIRE_WASM=1`, four-surface version lockstep) and **nothing enforces it**
- [x] ~~`sdk-gates.sh:80` unquoted heredoc~~ — **moot: the file no longer exists.** `scripts/` is now `publish-runtime.sh`, `version-lockstep.sh`, `wasm-size.sh`. Re-audit those three for the same quoting bug rather than re-filing this one
- [ ] The version-lockstep check from `TR-406` is a required job

## TR-607 · Close out the documentation debt
**Effort:** S · **Blocked by:** the wave

- [ ] The seven lying comments and the four bug-pinning tests are gone (`TR-135`)
- [ ] `wit/adapter.wit` is **wired or deleted** — today it is dead (no `build.rs`, no `wit-bindgen` in any manifest or in `Cargo.lock`; the only consumer is a dev-dep test asserting it parses) and badly drifted, re-creating the exact bug `Method::Custom` exists to prevent
- [ ] `crates/tropel-sdk/src/types.rs:910` still contains a literal NUL byte, so `grep`/`rg` classify the file as binary
- [ ] The README's wasm size figure is generated, not typed (`TR-404`)
- [ ] The 4 096 ceiling and the per-VU memory number are in the README with their measurement conditions
- [ ] `TROPEL_MASTER_TODO.md` is ticked to match reality — every task in this folder that closed, closed there too
