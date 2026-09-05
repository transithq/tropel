//! `tropel-auth` — HTTP request-signing primitives and pure OAuth2/JWT helpers.
//!
//! Two tiers live under one crate so native request engines and the
//! wasm32-unknown-unknown browser core share a single source of truth:
//!
//! * `signers` (feature-gated on `reqwest`, default-on) — the transport-coupled
//!   signers (`Basic`, `ApiKey`, `OAuth2`, `SigV4`, `OAuth1`, `Hawk`,
//!   `Digest`) that mutate a `reqwest::Request` in place. Reqwest cannot
//!   compile on wasm, so the browser slice disables this.
//! * `oauth` (always on) — pure, zero-I/O OAuth2 flow builders + RFC 7636 PKCE +
//!   JWT decode. This is what `tropel-core-wasm` consumes; it never touches
//!   reqwest/chrono, so it compiles to wasm32-unknown-unknown.

// dead_code is expected when the default features are off (the pure `oauth`
// module may expose items the embedder chooses not to call).
#![cfg_attr(not(feature = "reqwest"), allow(dead_code))]

/// Pure auth header builders — NOT behind the `reqwest` feature, so a
/// browser embedder can reach them. See the module docs for why.
pub mod builders;

#[cfg(feature = "reqwest")]
pub mod signers;

#[cfg(feature = "reqwest")]
pub use signers::{build_auth_signer, AuthSigner};

pub mod oauth;
