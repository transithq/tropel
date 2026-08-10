//! Postcard wire types for the browser slice C ABI (TROPEL_WASM_BUILD.md
//! Step 5A). `RunRequest` travels host → wasm; `RunOutcome` wasm → host.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tropel_runtime::IterationResult;
use tropel_sdk::config::ExpectedStatus;
// NOTE: Scenario is NOT on the wire — carried as scenario_json String.
// RunOutcome (IterationResult, Sample, TagMap) IS postcard-safe because
// TagMap's hand-rolled serde uses typed HashMap deserialization (not
// serde_json::Value → deserialize_any).

/// Everything needed to run one web scenario pass.
///
/// The scenario is carried as a JSON text string rather than embedding the
/// SDK [`Scenario`] type directly, because the SDK's config types use
/// JSON-oriented serde (`Body` routes through `serde_json::Value`, `AuthConfig`
/// is an internally-tagged enum, etc.) that cannot round-trip through postcard
/// (postcard refuses `deserialize_any`). The host (a JS API client) builds the
/// scenario from JS objects — serializing it as JSON text for the postcard wire
/// is the correct, zero-copy ABI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    /// The scenario to walk, JSON-encoded (items, scripts, variables, auth).
    /// The wasm side deserializes it with `serde_json::from_str` before
    /// running — 100% postcard-safe (just a string on the wire).
    pub scenario_json: String,
    /// VU id surfaced to `exec.vu.idInInstance()` / pm execution info.
    pub vu_id: u32,
    /// Scenario name surfaced to `exec.scenario.name` / pm.info.
    pub scenario_name: String,
    /// How many iterations to run (the host drives pacing, not this crate).
    pub iterations: u64,
    /// CLI/env variables merged into the variable scope.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,
    /// Expected status codes/ranges for `http_req_failed`, as SPEC STRINGS
    /// ("200", "200-399", "2xx") — NOT `Vec<ExpectedStatus>`.
    ///
    /// `ExpectedStatus` is `#[serde(untagged)]` (config.rs), and postcard
    /// refuses untagged enums (`deserialize_any` is a "never implement"
    /// feature). The F3 differential harness (native_vs_wasm.rs) caught this:
    /// any non-empty expected-status list made the wasm leg fail with
    /// "bad RunRequest postcard: This is a feature that PostCard will never
    /// implement" while the native leg ran fine. The host sends strings; the
    /// wasm side parses them into `ExpectedStatus` before running.
    #[serde(default)]
    pub expected_statuses: Vec<String>,
}

impl RunRequest {
    /// Parse the postcard-safe spec strings into `ExpectedStatus` values
    /// (numeric → `Single`, everything else → `Range`, mirroring
    /// [`ExpectedStatus::matches`]'s parsing).
    pub fn parsed_expected_statuses(&self) -> Vec<ExpectedStatus> {
        self.expected_statuses
            .iter()
            .map(|spec| match spec.trim().parse::<u16>() {
                Ok(code) => ExpectedStatus::Single(code),
                Err(_) => ExpectedStatus::Range(spec.clone()),
            })
            .collect()
    }
}

/// Result of a web run. `error` is set on a fatal (decode/build) failure;
/// per-iteration script failures are surfaced inside each `IterationResult`.
///
/// NOTE: do NOT add `skip_serializing_if` to any field here. Postcard is a
/// POSITIONAL format — `deserialize_struct` routes to
/// `deserialize_tuple(fields.len())`, which reads every field in order. A
/// skipped field (e.g. `error: None` with `skip_serializing_if`) makes the
/// encoder write a shorter payload than the decoder expects, and decode fails
/// with `DeserializeUnexpectedEnd`. The F3 differential harness caught this:
/// every individual sample round-tripped, but the full `RunOutcome` failed
/// exactly when `error` was `None`. `#[serde(default)]` alone is harmless
/// (decode-only) and keeps older/shorter payloads decodable.
#[derive(Debug, Serialize, Deserialize)]
pub struct RunOutcome {
    /// One `IterationResult` per requested iteration, in order.
    pub iterations: Vec<IterationResult>,
    /// Fatal error string, when the run could not complete. Always serialized
    /// (postcard is positional — see the struct doc).
    #[serde(default)]
    pub error: Option<String>,
}

impl RunOutcome {
    /// A failure outcome carrying only an error message.
    pub fn failed(msg: impl Into<String>) -> Self {
        Self {
            iterations: Vec::new(),
            error: Some(msg.into()),
        }
    }
}
