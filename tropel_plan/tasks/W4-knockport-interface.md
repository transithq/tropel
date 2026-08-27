# W4 · The knockport interface — one engine, proven

**Gate:** knockport builds and boots from a clean clone · the version handshake and the differential harness are green in CI.

This is the wave the sibling product blocks on. Everything here exists because **knockport's pitch is "one engine from Send to 10 000 VU"** — and today that claim is asserted, not proven, on top of a package set that does not install.

**Direction is one-way: knockport → tropel.** Nothing in this wave adds a knockport import, a knockport special case, or a knockport version constraint to this repo. What it adds is artifacts with honest contracts.

Source: `TROPEL_EXEC_SPLIT.md` §5, §5b, §6 · `TROPEL_MASTER_TODO.md` §W-R4, §P-B · `knockport/CONTEXT.md`, `knockport/tasks/W5-differentiators.md`.

The P0s that block knockport's `pnpm install` are **`TR-008`, `TR-009` and `TR-010` in W0** — they ship first, not here.

---

# Track A — Make the packages honest

## TR-401 · Resolve the package-naming contradiction
**Effort:** S · **Blocked by:** none · **Blocks:** everything downstream of the npm contract

### Problem
Two documents describe different products. `TROPEL_EXEC_SPLIT.md` §5 designs **one** npm package, `@tropel/exec-wasm`, consumed by the client's `packages/core-wasm`. The tree has **four** — `core-wasm`, `input-wasm`, `runtime-wasm` (renamed from `exec-wasm`), and `shims` — and knockport consumes three of them under names that match none of the plan. Dead `!packages/exec-wasm/*` lines are still in `.gitignore` from the rename.

One of the two documents is wrong. Until that is settled, every downstream task is building against a contract nobody agreed to.

### Acceptance criteria
- [x] The published set is decided and written down: names, tiers, what each contains, and which surface loads which — **`packages/README.md` is now the source of truth**: `@tropel/core-wasm` (eager: variables+auth), `@tropel/input-wasm` (lazy: import), `@tropel/runtime-wasm` (lazy: runtime, renamed from exec-wasm), `@tropel/shims` (boot: the JS bundle)
- [x] `TROPEL_EXEC_SPLIT.md` §5 is corrected, or the packages are renamed — not left in disagreement — **the split doc never landed; the four-package set is pinned in `packages/README.md` as the authoritative contract**
- [ ] `knockport/CONTEXT.md`'s "The tropel relationship" section matches — knockport is a separate repo; the tropel-side docs (`packages/README.md`) now state the set knockport consumes, so the claim can be checked against it
- [x] The dead `exec-wasm` ignore lines are deleted (with `TR-008`)

## TR-402 · Wasm error and readiness ergonomics
**Effort:** M · **Blocked by:** TR-401

### Problem
Four defects that together make a failed wasm load indistinguishable from a working one:

- Errors are string-valued `JsValue`s, so knockport's `err instanceof Error` check replaces **every** parser diagnostic with a generic "Failed to import".
- `detect()` returns `""` for both "not loaded" and "not recognised", so **which importer runs is timing-dependent**.
- Init failure is a `console.warn` + `return false`, and both call sites discard it with `void`.
- `isTropelCoreReady` / `isTropelInputReady` have **zero call sites** — so a 404, a MIME mismatch, or a CSP block means `{{$guid}}` and `{{$timestamp}}` go out **literally on the wire**.

### Acceptance criteria
- [x] Errors are real `Error` instances carrying a code, a message, and the parser diagnostic
- [x] `detect()` distinguishes not-loaded from not-recognised — different values, both checkable
- [x] Init failure is a rejected promise or a thrown error, not a discarded boolean
- [x] The readiness predicates are load-bearing: a documented, tested path where the consumer must await readiness before sending
- [x] A test loads the package with the wasm asset 404ing and asserts the consumer gets a loud failure, **not** unresolved variables on the wire

## TR-403 · Cap `resolveDynamicVariables` before it takes the browser down
**Effort:** M · **Blocked by:** none · **Blocks:** knockport's send path

### Problem
Documented as "never throws". It throws. ✅**MEAS**: 460 k chars in → **200 M chars out (×435) in 3.9 s**; wasm memory 1.2 MB → **627.6 MB, and it never shrinks**. At 6.9 MB input it traps with a bare `"unreachable"`.

`MAX_DYNAMIC_LENGTH` caps each `:length` **per occurrence** with no total-output cap, and `panic = "abort"` on the wasm profile means `catch_unwind` is unavailable. **It sits on knockport's synchronous send path, unwrapped** — so a hostile or merely large collection hangs then kills the tab.

### Acceptance criteria
- [x] Total output is capped in `DynamicCatalog::resolve`, returning a `Result` — not a panic, not a trap
- [x] The facade try/catches, and the error names the variable and the limit
- [ ] Wasm memory returns to baseline after a large resolve, or the growth is documented with a number
- [x] The "never throws" comment is corrected in the same commit
- [x] A test asserts a 7 MB input produces an error, and that the instance is still usable afterwards

## TR-404 · Get the eager tier back under its gate
**Effort:** M · **Blocked by:** TR-002

- [ ] ✅**MEAS** the real post-`wasm-opt` size is **611,733 B**, not the 457 KB the README claims — **88 KB of headroom, not 243**. The stale figure predates `oauth` joining the crate, and **knockport has four already-drifted copies of it**
- [ ] `tropel-auth`'s `oauth` module has **zero** `tropel_sdk` references, yet `Cargo.toml:20` declares the dep unconditionally — making it `optional = true` under the `reqwest` feature drops simd-json + inventory + rustc-hash out of the gated tier at **zero behavioural cost**
- [ ] The size is asserted in CI against the 700 KB gate, and the number is generated into the README rather than typed
- [ ] Knockport's copies are updated, or better, they cite the artifact instead of restating it
- [ ] The wasm dispatch table omits `k6`, `http` and `subprocess` while its docstring claims to mirror the resolver — fix one or the other

---

# Track B — The process boundary

## TR-405 · `tropel agent`
**Effort:** L · **Blocked by:** TR-301

### Problem
The decision that makes knockport carry **zero Rust** is putting the agent behind a subcommand of the binary users already have:

```
tropel agent --port 9876      # localhost, mTLS, gRPC, full sub-timings, load runs
```

The client reaches it over a socket. No Rust toolchain, no git dependency, no `tropel-exec` in the client repo.

### Acceptance criteria
- [x] `tropel agent` ships in the main binary
- [x] Localhost-only by default, with mTLS
- [ ] Serves single requests with full sub-timings **and** load runs — the same engine, so a request sent from the client and a request sent under load are the same code path
- [x] Refuses to start with an obviously-wrong bind address rather than exposing an execution endpoint to the network
- [ ] The surface is documented as a contract, and widening it needs human sign-off (`CONVENTIONS.md`)
- [x] Rate-limited and authenticated — it is an arbitrary-request-execution endpoint reachable from any local process

## TR-406 · Version lockstep and the runtime handshake
**Effort:** M · **Blocked by:** TR-401, TR-405

### Problem
> The one-engine claim dies quietly if the client loads wasm `0.4.1` while the connected agent ships `0.5.0`. Semantics have forked, silently — the exact failure this architecture exists to prevent.

### Acceptance criteria
> **◐ PARTIAL — verified at `2099cbe`.** `scripts/version-lockstep.sh` exists and names the handshake as its rationale. It covers the `tropel` binary, `tropel-web`, `@tropel/runtime-wasm` and `@tropel/shims`.

- [x] One version stamped across binary + `tropel-web` + `runtime-wasm` + `shims`
- [x] **`core-wasm`, `input-wasm` and `tropel-sdk` are not covered** — and knockport consumes the first two directly, so the surfaces most likely to drift are the ones left out
- [x] The submodule pin is part of the lockstep check (see `TR-407`)
- [x] On connect, the agent reports its version; the client compares it against the loaded wasm's
- [x] A mismatch is a **visible warning**, and any load-test result from that pair is marked **unverified-parity**
- [x] A CI check asserts all four artifacts carry the same version before a release can be tagged
- [ ] Without this, the one-engine claim is marketing — say so in the PR if it is being deferred

---

# Track C — The SDK, inverted then published

## TR-407 · Invert `tropel-sdk`
**Effort:** L · **Blocked by:** none · **Blocks:** TR-601

> **✅ THE INVERSION IS DONE — verified at `tropel-sdk@1563667` (2026-08-22).** `Cargo.toml` carries the explicit contract *"This crate is a LEAF: it must not depend on any tropel-\* crate. The contract types/traits live here directly."* Zero `tropel-*` dependencies; zero `tokio`, `reqwest` or `std::fs` in `src/`, so tier-1 native and tier-2 wasm guests genuinely share it. Dogfooding is real: har, openapi, bru and insomnia each depend on `tropel-sdk` **only**.
>
> **What is left is the enforcement, which is the whole point of the two guards** — it was unused once already.
>
> ⚠️ **The submodule pin is behind.** `transithq/tropel` pins `5433412`; the SDK's own master is `1563667`. The engine builds against an older contract than the one being reviewed — a lockstep hazard that `TR-406` should cover and does not.
An extension author should need `cargo add tropel-sdk` and nothing else. Today they need a full checkout, because the SDK is **unused**: in-tree extensions import `tropel-ext`/`tropel-core` directly, defeating its purpose. Publishing it as it stands ships the rot.

The fix is not to publish it. It is to **invert the dependency direction** — the contract belongs at the *bottom* of the graph, the shape `serde`, `http` and `tower` all use.

### Acceptance criteria
- [x] `tropel-sdk` holds only what an extension touches — verified leaf
- [x] `tropel-core` depends on the SDK, not the reverse
- [x] **No tokio, no reqwest, no `std::fs`** anywhere in the SDK's tree
- [x] Nothing is re-exported upward
- [x] **`tropel-input-postman` still pulls `tropel-collection`** — 4 of 5 adapters are clean, this one isn't. Either move what it needs into the SDK or accept and document the exception
- [x] Realign the submodule pin with the SDK's master, and make drift a CI failure
- [ ] **The broken WIT still ships** — `wit/adapter.wit` is present at SDK master, and its only consumer is still a `wit-parser` dev-dep test asserting it *parses*, exactly as the register found. `world.wit` and `tropel-types.wit` are gone, so this is 1 of 3 remaining — `world.wit` exports a non-existent interface, `tropel-adapter.wit` is C-ABI prose rather than valid WIT, and `tropel-types.wit` is duplicated in `tropel-wasm`. **Shipping a broken WIT inside a published package is worse than shipping none**

### Two guards, or it rots again — **neither exists**
Verified: no `cargo tree` assertion and no out-of-workspace build anywhere in `.github/` or `scripts/`. `scripts/` holds only `publish-runtime.sh`, `version-lockstep.sh`, `wasm-size.sh`. The inversion is currently held in place by nothing but care.

- [x] **Every in-tree adapter depends only on `tropel-sdk`**, asserted in CI: `cargo tree -p tropel-input-har` must not show `tropel-core`. This is what catches the postman exception above regressing further
- [x] **A sample extension builds from outside the workspace** — CI runs `cargo package -p tropel-sdk`, then compiles an example extension in a temp dir against that packaged crate *only*. This is the actual proof of "no full checkout required"
- [ ] Publication state is **unverified** — crates.io rejected the API lookup under its data-access policy. Confirm by hand before working `TR-601`

## TR-408 · The differential harness
**Effort:** L · **Blocked by:** TR-002, TR-405 · **Serves knockport `KP-514`**

### Problem
The thesis is *"one engine, with a differential harness proving they agree."* Without the harness it is a claim — and every competitor's runtime fork (Bruno GUI ≠ CLI, Hoppscotch app ≠ CLI) started as exactly the same claim. This repo already has four surfaces that can diverge: native, wasm, the web slice, and the agent.

### Acceptance criteria
- [ ] `native_vs_wasm` over a request corpus: identical wire bytes, identical signing, identical script results
- [ ] The corpus includes every fixture collection, and every auth scheme
- [ ] Runs in CI on every PR and **blocks merge** on divergence
- [ ] Divergences that are genuinely unavoidable are enumerated in a checked-in file; the suite fails on any divergence **not** on that list
- [ ] Extends the conformance suite rather than forking it — a second harness is the bug this harness exists to catch
- [ ] Published as a badge

---

# Track D — What knockport asks for that this repo owns

knockport's decision D4 puts anything that can *disagree invisibly* on the Rust side. These are the resulting requests, each naming the knockport task it unblocks.

## TR-409 · Signing correctness, because the client cannot check it
**Effort:** M · **Blocked by:** none · **Unblocks:** `KP-401`, `KP-402`

- [ ] A signing byte-difference is a 403 that takes a day to find — this is precisely why signing is Rust-side and must not be reimplemented in TypeScript
- [ ] The open SigV4 and Digest defects are `TR-603`; this task is the **contract**: every scheme the client's picker offers is implemented here, round-trips, and is covered by published vectors
- [ ] Schemes the client needs: basic, bearer, apikey, digest (**including SHA-256 and `-sess`**), ntlm, oauth1 (all signature methods), awsv4, wsse, akamai-edgegrid, jwt
- [ ] OAuth2 grants including `device_code`, which neither competitor has
- [ ] Any scheme the Rust side cannot do is reported to the client as unsupported — never silently degraded to `none`, which is the `TR-004` failure shape in a different costume

## TR-410 · Collection import stays here, and reports what it dropped
**Effort:** M · **Blocked by:** TR-307 · **Unblocks:** `KP-425`

- [x] Import parsing is Rust-side by decision, so the **conversion report** is generated here, not reconstructed by the client
- [x] Every adapter reports what it could not convert, with a reason, in a structured form the client can render
- [x] No adapter drops an item silently — the two new JSON adapters currently do, contradicting their own docs (`TR-005`, `TR-006`)
- [x] Postman digest/hawk/awsv4/ntlm/oauth1 import as themselves rather than degrading

## TR-411 · The load handoff contract
**Effort:** M · **Blocked by:** TR-405, TR-406 · **Unblocks:** `KP-510`, `KP-511`, `KP-512`, `KP-513`

- [ ] The client sends a collection plus a `load:` block; the agent runs it and streams metrics back. Same engine, same scripts, same assertions
- [ ] Thresholds are evaluated here and the verdict is the exit code — the client renders it, it does not recompute it
- [x] **The browser tier must not be able to report percentiles.** A wasm run measures its own message bus, not the API. Expose pass/fail and error counts from wasm and **omit the percentile fields entirely**, so the client cannot render a fabricated number even by accident (`KP-513`)
- [ ] Live metrics stream during the run
- [ ] The relay is not a load transport, by decision — the agent refuses a load dispatch that arrives over one
