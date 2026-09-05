# Native vs wasm differential divergences (TR-408)

This file enumerates the divergences between the native and wasm32 runtime that
are GENUINELY unavoidable — the `native_vs_wasm` differential harness must
either not exercise them, or the divergence must be listed here with a reason
before the CI merge-block is waived.

Current state: **no known unavoidable divergences.** The harness (`crates/
tropel-web/tests/native_vs_wasm.rs`) runs the identical `ScenarioRunner` code
compiled two ways (host vs wasm32-wasip1) with a shared deterministic fixture,
and asserts the normalized outcomes are byte-identical across:

- methods (GET/POST), JSON/query/form bodies
- **credential-copy** auth (bearer, basic, api-key, digest, oauth2) — the
  header is a near-verbatim copy of the credential
- **computed-signature** auth (OAuth1 HMAC-SHA1, AWS SigV4, Hawk) — these sign
  over the method, URL, query string and body, so a canonicalisation
  difference between the legs shows up here and nowhere else. They are the
  schemes where a byte difference is a 403 that costs a day to find, which is
  the stated reason signing lives in Rust rather than being reimplemented in
  TypeScript (TR-409). The SigV4 case deliberately uses a URL with consecutive
  slashes and an encoded query value — TR-603's slash-normalisation bug lived
  exactly there
- error statuses (404, 500), redirects (301), text/empty bodies
- a throwing test script (both legs report the same `script_failures`)

**Still uncovered:** NTLM, WSSE, JWT and Akamai EdgeGrid. Those are reported as
`unsupported` by `build_auth_signer` rather than implemented (TR-409), so there
is no signature for the two legs to disagree about yet. Add them here when they
land.

If a future divergence appears, add it here with the reason it is unavoidable
(e.g. a host-only feature like `std::time` precision, or a platform-specific
timestamp) AND mark the corresponding corpus item `#[ignore]` with a link to
this file. A divergence that is NOT listed here is a bug in the one-engine
claim and must be fixed, not waived.

## The resolution corpus, and its third leg (KT-202)

`packages/core-wasm/fixtures/resolve-corpus.json` is a separate, narrower
differential: the 16 cases that motivated moving the `{{var}}` resolver into
Rust in the first place. It now runs through **three** paths, all reading the
same bytes rather than restating the cases:

| leg | where | serves |
|---|---|---|
| native Rust | `tropel-core-wasm`'s `conformance_corpus` | the engine |
| real wasm | `packages/core-wasm/smoke.mjs` | KnockPort's **browser** tier |
| the agent, over a socket | `tropel-engine`'s `the_resolution_corpus_agrees_over_the_socket` | KnockPort's **desktop** tier (KP-209) |

The third leg exists because since KP-209 the desktop tier resolves through
the agent, so an agent that handled `{{base-url}}` differently would put
literal text on the wire **for desktop users only** — the exact divergence
this corpus was written to catch, in the one tier it was not yet checked in.

It deviates from KT-202's literal wording ("run the corpus through the
TypeScript host"). Driving KnockPort's TypeScript from a Rust test would need
Node and a second checkout inside tropel's CI; the agent leg covers what that
was for with no cross-repo dependency, and `smoke.mjs` already **is** the
TypeScript leg for the browser tier.

Current state: **no divergences.** All 16 cases agree across all three.
