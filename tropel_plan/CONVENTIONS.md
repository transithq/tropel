# CONVENTIONS — how to execute these tasks

## Evidence grades

Carry these forward from the register. A claim without a grade is an opinion.

| Tag | Meaning |
|---|---|
| ✅**EXEC** | Verified by running code — a Node VM for JS, a compiled QuickJS harness for the engine, a scratch consumer crate for the SDK |
| ✅**CALC** | Recomputed numerically — crypto against published vectors, `$ref` fanout, rounding errors |
| ✅**MEAS** | Measured on hardware, with the machine stated |
| **READ** | Source-verified, not executed |

**Every performance claim in a PR must be ✅MEAS against a committed benchmark.** Until `TR-002` lands, no perf task can be closed — the numbers are unverifiable by construction.

## Definition of done

A task is done when **all** of these hold:

1. Acceptance criteria are met, each one individually verifiable.
2. Tests listed under **Tests required** exist and pass.
3. **A test exists that fails on the pre-fix code, asserting the user-visible number** — the threshold verdict, the summary line, the exit code — not the internal call. If you cannot write one, say so in the PR and explain why.
4. No invariant in `CONTEXT.md` is broken.
5. `cargo build --release`, `cargo test`, and `cargo clippy -- -D warnings` pass from a clean clone.
6. **The twin is checked.** Three of the four original P0s were previously-fixed defects that reappeared on a path the first fix didn't cover. Grep for every sibling call site and say in the PR which ones you checked.
7. **The register is ticked.** Mark the corresponding item in `TROPEL_MASTER_TODO.md`, or the next review round re-files it.
8. Comments touching changed behaviour are corrected in the same commit.

## Branch and PR format

Note : Don't ever add AI/AI Agent attribution in contributors in the commit message.

```
branch:  tr/<task-id>-<short-slug>          e.g. tr/TR-004-expected-status-parse
commit:  <type>(<scope>): <subject>  [TR-004]
PR body:
  ## What            one paragraph
  ## Task            TR-004
  ## Acceptance      the task's checklist, ticked
  ## Tests           what was added, and what it would have caught
  ## Twin check      the sibling call sites you grepped, and what you found
  ## Evidence        EXEC / CALC / MEAS / READ, with the command or the machine
  ## Risk            what could break, and how it was checked
```

Types: `feat` `fix` `refactor` `test` `docs` `chore` `perf`.

## One task per PR

Do not bundle. If a task needs a prerequisite that isn't listed, **stop and open the prerequisite as its own PR first**, then note the dependency so the roadmap can be corrected.

The exception is the **"one change closes many"** set in the register — where one change is *documented* to close a list of items, that list ships together, and the PR body enumerates it.

## Tests — non-negotiables

- **Assert the user-visible number.** A test that asserts an internal call still passes when the summary lies.
- **When fixing a bug a test currently pins as correct, delete or invert that test in the same PR** and say so in the PR body. Four are known:

  | Test | Pins |
  |---|---|
  | *(table cleared — the four known bug-pinning tests were inverted or deleted as their fixes landed: SigV4 canonical URI now asserts correct normalization at `signers.rs:1581-1584`, `protocolProfileBehavior` moved to item level, `last`-hardcoded-0 inverted, `degrade_to_status0_error` removed)* |

- **Never assert `f(x) === f(x)`.** That tests determinism, not correctness.
- **A test must exercise the production code path.** Several defects survived because tests used a tree deserializer while production used a streaming one, or asserted a `#[cfg(test)]` twin of the real function.
- **Conformance suites run against every surface** — native, wasm, CLI, and the agent. If two can diverge, one suite runs against both.
- **Smoke tests must be able to fail.** `smoke.mjs` could not detect a re-introduced wire P0; the `input-wasm` smoke has the exact blind spot the `runtime-wasm` one was fixed for. A smoke test that only asserts shape is decoration.

## Verify before you close

**Commit subjects over-claim in this repo.** A commit named `fix/openapi-type-array-ref` added a reader that can never run; a `ci:` commit added no CI. Read the tree, recompute the vectors, run the binary. The register's `Status` sections are explicitly *"claimed by commit subject — verify before closing"*.

Equally: **do not re-file corrected claims.** `TROPEL_MASTER_TODO.md` closes with a *Corrections — do not re-file* table. The recurring ones:

- h2 is **on by default** — that's the problem, not the gap.
- `panic = "abort"` **breaks the VU pool** — `BusyGuard`, `StopOnDrop` and `JoinError` all need unwinding.
- `target-cpu=native` is not viable — the distributed mode ships binaries to other machines.
- `find_idle_slot` (59 ms per 10 000-VU ramp), the TUI (2 Hz, 0.005 % of a core), and hot-path `Regex::new` (none exists) are **not** problems.
- `discardResponseBodies` is implemented, and correctly.
- CryptoJS ships **zero** constant tables — it is already a native dispatcher.

## What needs a human before merge

Stop and ask on any of these:

- **Publishing** — anything reaching crates.io, npm, or GitHub Releases. Versions are permanent.
- **Changing the metric contract** — renaming or removing a metric, changing a tag's default set, changing what `http_req_duration` sums. Every dashboard downstream breaks silently.
- **Changing the wire format** between controller and agent, or the wasm ABI.
- **Widening the control API or the agent's surface** — it is a localhost-reachable execution endpoint.
- **Adding a dependency over ~100 KB, or any new transitive crypto dependency.**
- **Anything that changes what leaves the user's machine.**
- **Removing or renaming a public `tropel-sdk` item** once published, pre-1.0 semver notwithstanding.

## Performance budgets — enforce in CI, fail the build

| Budget | Limit | Today |
|---|---|---|
| Eager wasm tier, post-`wasm-opt` | **700 KB** | **see `packages/core-wasm/README.md`** — the figure is generated by `scripts/build.sh` and asserted in CI. Do not restate it here: this row said 611,733 B while the generated value was 569,352 B, recreating the drift TR-404 existed to remove |
| Lazy import tier | 1.5 MB | — |
| QuickJS heap per VU, before user script | **900 KB (TR-501/TR-503)** | **497,584 B** ✅MEAS (`malloc_size`, release, Apple Silicon; bare context 104,768 B) · ~4.6 GB at 10k. `TR-503`'s shared `Runtime` is **NOT implemented** — the 57 KB figure was never measured, see `TR-503`. Reproduce: `cargo test -p tropel-engine --release measure_per_vu_quickjs_heap -- --nocapture --ignored` |
| Egress, sustained | 100 k samples/s with the drop counter at **zero** | **OTLP: 123.76 ms per 100 ms window ✅MEAS** (6.2× the 20 ms budget; max ~80.8 k samples/s) — gzip fixed the wire, not the CPU; protobuf still open (TR-304). The 4-way aggregator shard is real, but the aggregator is not the binding constraint — the output encoder is |
| In-flight concurrency | **10,000** — `MAX_WORKERS=10k` + async `sleep` Promise (`TR-502`) | 10,000 via 10k workers, sleep yields via `tokio::time::sleep` + job-queue, `pids.max` caps in containers |
| Aggregator duty cycle | < 20 % | ~45 % — `build_results` throttles load generation to ~55 % |

## Style

- No `unwrap_or` that can turn a parse failure into a valid-looking value. `ExpectedStatus` is the cautionary tale: one typo produces a perfect green run against a server returning nothing but 500s.
- `let _ =` on a registration or send is a silent failure. There are 103 in non-test k6 driver code.
- Comments explain **why**, and name the failure they prevent. This tree does that unusually well — which is exactly why a stale one is dangerous.
- Prefer deleting code to adding a flag.
