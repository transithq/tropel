//! # OTLP/HTTP protobuf wire encoder (TR-304)
//!
//! Encodes an `ExportMetricsServiceRequest` directly onto a `Vec<u8>` in
//! protobuf binary format, for `Content-Type: application/x-protobuf`.
//!
//! ## Why hand-rolled and not `prost` message structs
//!
//! `CONVENTIONS.md` gates dependencies over ~100 KB on human sign-off.
//! `prost` (192 KB) + `prost-derive` (140 KB) are already in the tree via
//! `tropel-x-grpc`, so a `prost::Message` derive would not have added a new
//! *crate* — but it would have added ~330 KB of dependency to a crate that
//! ships in every build, including the `--no-default-features` browser
//! slice, and it would have coupled the OTLP output to the gRPC extension's
//! feature-gating (`TR-313` gates `protox`/`tonic` off).
//!
//! It would also have been **slower**, which is the entire point of the
//! task. `prost`'s derived encoder needs owned fields: every OTLP
//! `KeyValue` is `{ key: String, value: Option<AnyValue> }`, so a window of
//! 10 000 samples × 7 tags is ~210 000 heap allocations before a single
//! byte reaches the wire. Writing straight from the `&str` borrowed out of
//! the sample's `Arc<TagMap>` allocates nothing per attribute.
//!
//! `prost` IS used — as a **dev-dependency only** — to decode what this
//! module produces in the round-trip test, so the wire bytes are checked
//! against a third-party protobuf implementation rather than against our
//! own reader. See `tests::round_trip_decodes_under_prost`.
//!
//! ## Schema
//!
//! Field numbers are from `opentelemetry-proto` v1 (the same numbers the
//! `opentelemetry-proto` crate's generated code carries) and are pinned by
//! [`tests::golden_wire_bytes`], which asserts an exact hand-derived byte
//! string:
//!
//! ```text
//! ExportMetricsServiceRequest { repeated ResourceMetrics resource_metrics = 1; }
//! ResourceMetrics  { Resource resource = 1; repeated ScopeMetrics scope_metrics = 2; }
//! Resource         { repeated KeyValue attributes = 1; }
//! ScopeMetrics     { InstrumentationScope scope = 1; repeated Metric metrics = 2; }
//! InstrumentationScope { string name = 1; }
//! Metric           { string name = 1; ... oneof data { Gauge gauge = 5; Sum sum = 7; } }
//! Gauge            { repeated NumberDataPoint data_points = 1; }
//! Sum              { repeated NumberDataPoint data_points = 1;
//!                    AggregationTemporality aggregation_temporality = 2;
//!                    bool is_monotonic = 3; }
//! NumberDataPoint  { fixed64 start_time_unix_nano = 2; fixed64 time_unix_nano = 3;
//!                    oneof value { double as_double = 4; }
//!                    repeated KeyValue attributes = 7; }
//! KeyValue         { string key = 1; AnyValue value = 2; }
//! AnyValue         { oneof value { string string_value = 1; } }
//! ```
//!
//! Note the two timestamps and `as_double` are **fixed64 / double** (wire
//! type 1, eight little-endian bytes), NOT varints. Encoding them as
//! varints produces a payload a collector silently misreads.
//!
//! Fields left at their proto3 default (`description`, `unit`,
//! `schema_url`, `flags`, `dropped_attributes_count`) are omitted. Omission
//! and an explicit default are the same value on the wire; the JSON path
//! writes `"description": ""` for the same reason.

use std::collections::HashMap;
use std::time::UNIX_EPOCH;
use tropel_sdk::types::{Sample, SampleType};

/// `service.name` resource attribute value, matching the JSON encoder.
const SERVICE_NAME: &str = "tropel";
/// `InstrumentationScope.name`, matching the JSON encoder.
const SCOPE_NAME: &str = "tropel";

// ── protobuf wire primitives ──────────────────────────────────────────
//
// Wire types: 0 = varint, 1 = 64-bit, 2 = length-delimited, 5 = 32-bit.

const WIRE_VARINT: u32 = 0;
const WIRE_64BIT: u32 = 1;
const WIRE_LEN: u32 = 2;

#[inline]
fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// Encoded width of `v` as a varint, in bytes. Used to size a nested
/// message's length prefix without encoding it twice.
///
/// Seven payload bits per byte, so `ceil(significant_bits / 7)`; zero still
/// costs one byte. Every nested length prefix is computed from this, so an
/// off-by-one here desynchronises the whole stream —
/// `tests::varint_len_matches_encoded_width` pins it against what
/// [`put_varint`] actually writes.
#[inline]
fn varint_len(v: u64) -> usize {
    let bits = 64 - v.leading_zeros() as usize;
    if bits == 0 {
        1
    } else {
        bits.div_ceil(7)
    }
}

#[inline]
fn put_tag(buf: &mut Vec<u8>, field: u32, wire: u32) {
    put_varint(buf, ((field << 3) | wire) as u64);
}

#[inline]
fn put_str_field(buf: &mut Vec<u8>, field: u32, s: &str) {
    put_tag(buf, field, WIRE_LEN);
    put_varint(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

/// Write an already-encoded nested message `payload` as length-delimited
/// field `field`.
#[inline]
fn put_msg_field(buf: &mut Vec<u8>, field: u32, payload: &[u8]) {
    put_tag(buf, field, WIRE_LEN);
    put_varint(buf, payload.len() as u64);
    buf.extend_from_slice(payload);
}

#[inline]
fn put_fixed64(buf: &mut Vec<u8>, field: u32, v: u64) {
    put_tag(buf, field, WIRE_64BIT);
    buf.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_double(buf: &mut Vec<u8>, field: u32, v: f64) {
    put_tag(buf, field, WIRE_64BIT);
    buf.extend_from_slice(&v.to_bits().to_le_bytes());
}

#[inline]
fn put_varint_field(buf: &mut Vec<u8>, field: u32, v: u64) {
    put_tag(buf, field, WIRE_VARINT);
    put_varint(buf, v);
}

// ── KeyValue attributes ───────────────────────────────────────────────
//
// `KeyValue { key = 1; AnyValue value = 2 }` wrapping
// `AnyValue { string_value = 1 }`. Both inner field numbers are < 16, so
// their tags are exactly one byte — that is where the literal `1`s in the
// length arithmetic below come from.

/// Encoded size of one `KeyValue` attribute **including** its own field tag
/// and length prefix, so a caller can sum it into a parent's length without
/// encoding the attribute first.
#[inline]
fn attr_encoded_len(field: u32, key: &str, value: &str) -> usize {
    let any = 1 + varint_len(value.len() as u64) + value.len();
    let kv = 1 + varint_len(key.len() as u64) + key.len() + 1 + varint_len(any as u64) + any;
    tag_len(field) + varint_len(kv as u64) + kv
}

#[inline]
fn tag_len(field: u32) -> usize {
    varint_len(((field << 3) | WIRE_LEN) as u64)
}

/// Write `key`/`value` as a `KeyValue` attribute under `field`.
///
/// Borrows straight out of the sample's `Arc<TagMap>` — no `String` is
/// materialised per attribute, which is the allocation the `serde_json`
/// path pays 14 times per sample.
#[inline]
fn put_attr(buf: &mut Vec<u8>, field: u32, key: &str, value: &str) {
    let any = 1 + varint_len(value.len() as u64) + value.len();
    let kv = 1 + varint_len(key.len() as u64) + key.len() + 1 + varint_len(any as u64) + any;
    put_tag(buf, field, WIRE_LEN);
    put_varint(buf, kv as u64);
    put_str_field(buf, 1, key); // KeyValue.key
    put_tag(buf, 2, WIRE_LEN); // KeyValue.value: AnyValue
    put_varint(buf, any as u64);
    put_str_field(buf, 1, value); // AnyValue.string_value
}

// ── NumberDataPoint ───────────────────────────────────────────────────

/// `NumberDataPoint` field numbers.
const DP_START_TIME: u32 = 2; // fixed64
const DP_TIME: u32 = 3; // fixed64
const DP_AS_DOUBLE: u32 = 4; // double
const DP_ATTRIBUTES: u32 = 7; // repeated KeyValue

/// Bytes a fixed64/double field costs: one-byte tag (all field numbers
/// here are < 16) plus eight payload bytes.
const FIXED64_FIELD_BYTES: usize = 9;

#[inline]
fn nanos_since_epoch(s: &Sample) -> u64 {
    s.timestamp
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Total encoded size of a data point's attribute list, tags and length
/// prefixes included.
///
/// `TagMap::iter` is not `Clone`, so the length pass and the write pass each
/// take a fresh iterator rather than one being a clone of the other.
#[inline]
fn attrs_encoded_len<'a>(attrs: impl Iterator<Item = (&'a str, &'a str)>) -> usize {
    attrs
        .map(|(k, v)| attr_encoded_len(DP_ATTRIBUTES, k, v))
        .sum()
}

/// Append one `NumberDataPoint` to `buf` as repeated field `1` of a
/// `Gauge`/`Sum`.
///
/// `attrs_len` must be [`attrs_encoded_len`] of the same attributes —
/// the length prefix is written before the payload, so it cannot be
/// backfilled. A `debug_assert` catches any drift.
///
/// `start_time` is `Some` only for DELTA Sums — a DELTA point without
/// `start_time_unix_nano` is dropped silently by the Collector's
/// `deltatocumulative` and Prometheus exporters (see the JSON encoder's
/// note; this is the same invariant on the protobuf wire).
fn put_data_point<'a>(
    buf: &mut Vec<u8>,
    start_time: Option<u64>,
    time: u64,
    value: f64,
    attrs_len: usize,
    attrs: impl Iterator<Item = (&'a str, &'a str)>,
) {
    let mut len = FIXED64_FIELD_BYTES * 2 + attrs_len; // time + as_double + attrs
    if start_time.is_some() {
        len += FIXED64_FIELD_BYTES;
    }

    put_tag(buf, 1, WIRE_LEN);
    put_varint(buf, len as u64);
    #[cfg(debug_assertions)]
    let start_of_payload = buf.len();

    if let Some(st) = start_time {
        put_fixed64(buf, DP_START_TIME, st);
    }
    put_fixed64(buf, DP_TIME, time);
    put_double(buf, DP_AS_DOUBLE, value);
    for (k, v) in attrs {
        put_attr(buf, DP_ATTRIBUTES, k, v);
    }

    #[cfg(debug_assertions)]
    debug_assert_eq!(
        buf.len() - start_of_payload,
        len,
        "NumberDataPoint length prefix disagrees with the bytes written — \
         a length-prefix bug produces a payload a collector cannot parse"
    );
}

// ── ExportMetricsServiceRequest ───────────────────────────────────────

/// Build an OTLP/HTTP **protobuf** `ExportMetricsServiceRequest` from
/// buffered metrics.
///
/// Semantics are identical to the JSON encoder in [`crate::otlp`]: counters
/// aggregate per (metric, sorted tag-set) within the flush window and ship
/// as a monotonic `Sum` with **DELTA** temporality carrying
/// `start_time_unix_nano`; everything else ships as a `Gauge` with one data
/// point per raw observation.
pub fn build_export_request_protobuf(metrics: &HashMap<String, Vec<Sample>>) -> Vec<u8> {
    // ~80 bytes per data point at 7 short tags; one growth doubling at most.
    let hint: usize = metrics.values().map(|v| v.len()).sum::<usize>() * 96 + 64;
    let mut metrics_blob: Vec<u8> = Vec::with_capacity(hint);

    // Reused across metrics so the per-metric scratch does not re-allocate.
    let mut data_payload: Vec<u8> = Vec::new();
    let mut metric_payload: Vec<u8> = Vec::new();
    let mut tag_key: Vec<(&str, &str)> = Vec::new();

    for (name, samples) in metrics {
        let is_counter = samples.iter().any(|s| s.sample_type == SampleType::Counter);
        data_payload.clear();

        if is_counter {
            // Sum per sorted tag-set, keeping the LAST timestamp seen for a
            // tag-set and the EARLIEST across the whole metric as the delta
            // window start. Keys borrow out of each sample's `Arc<TagMap>`,
            // so grouping allocates only for a genuinely new tag-set — the
            // JSON path allocates two `String`s per tag per sample.
            let mut earliest_ts: u64 = u64::MAX;
            let mut per_tags: HashMap<Vec<(&str, &str)>, (f64, u64)> = HashMap::new();
            for s in samples {
                tag_key.clear();
                tag_key.extend(s.tags.iter());
                tag_key.sort_unstable();
                let ts = nanos_since_epoch(s);
                if ts < earliest_ts {
                    earliest_ts = ts;
                }
                match per_tags.get_mut(&tag_key) {
                    Some((sum, last)) => {
                        *sum += s.value;
                        *last = ts;
                    }
                    None => {
                        per_tags.insert(tag_key.clone(), (s.value, ts));
                    }
                }
            }
            for (tags, (sum, ts)) in &per_tags {
                let start = if earliest_ts != u64::MAX {
                    earliest_ts
                } else {
                    *ts
                };
                put_data_point(
                    &mut data_payload,
                    Some(start),
                    *ts,
                    *sum,
                    attrs_encoded_len(tags.iter().copied()),
                    tags.iter().copied(),
                );
            }
            // Sum.aggregation_temporality = 1 (DELTA), Sum.is_monotonic = true.
            // Written after the data points so `data_payload` is the complete
            // `Sum` message payload.
            put_varint_field(&mut data_payload, 2, 1);
            put_varint_field(&mut data_payload, 3, 1);
        } else {
            for s in samples {
                put_data_point(
                    &mut data_payload,
                    None,
                    nanos_since_epoch(s),
                    s.value,
                    attrs_encoded_len(s.tags.iter()),
                    s.tags.iter(),
                );
            }
        }

        metric_payload.clear();
        put_str_field(&mut metric_payload, 1, name); // Metric.name
        put_msg_field(
            &mut metric_payload,
            if is_counter { 7 } else { 5 }, // oneof data: sum = 7, gauge = 5
            &data_payload,
        );
        put_msg_field(&mut metrics_blob, 2, &metric_payload); // ScopeMetrics.metrics
    }

    // The two outer wrappers are singletons, so their lengths are arithmetic
    // on `metrics_blob.len()` — the payload is copied exactly once.
    let mut scope = Vec::with_capacity(16);
    put_str_field(&mut scope, 1, SCOPE_NAME); // InstrumentationScope.name

    let mut resource = Vec::with_capacity(48);
    put_attr(&mut resource, 1, "service.name", SERVICE_NAME); // Resource.attributes

    let scope_field_bytes = 1 + varint_len(scope.len() as u64) + scope.len();
    let scope_metrics_len = scope_field_bytes + metrics_blob.len();
    let resource_field_bytes = 1 + varint_len(resource.len() as u64) + resource.len();
    let resource_metrics_len =
        resource_field_bytes + 1 + varint_len(scope_metrics_len as u64) + scope_metrics_len;

    let header_hint = resource_metrics_len - metrics_blob.len() + 8;
    let mut out = Vec::with_capacity(metrics_blob.len() + header_hint);
    put_tag(&mut out, 1, WIRE_LEN); // ExportMetricsServiceRequest.resource_metrics
    put_varint(&mut out, resource_metrics_len as u64);
    put_msg_field(&mut out, 1, &resource); // ResourceMetrics.resource
    put_tag(&mut out, 2, WIRE_LEN); // ResourceMetrics.scope_metrics
    put_varint(&mut out, scope_metrics_len as u64);
    put_msg_field(&mut out, 1, &scope); // ScopeMetrics.scope
    out.extend_from_slice(&metrics_blob); // ScopeMetrics.metrics (pre-tagged)

    debug_assert_eq!(
        out.len(),
        1 + varint_len(resource_metrics_len as u64) + resource_metrics_len,
        "top-level length prefix disagrees with the bytes written"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tropel_sdk::types::TagMap;

    fn sample_at(
        metric: &str,
        value: f64,
        sample_type: SampleType,
        tags: &[(&str, &str)],
        nanos: u64,
    ) -> Sample {
        let mut map = TagMap::new();
        for (k, v) in tags {
            map.insert(*k, *v);
        }
        Sample {
            metric: std::borrow::Cow::Owned(metric.to_string()),
            value,
            tags: Arc::new(map),
            timestamp: UNIX_EPOCH + Duration::from_nanos(nanos),
            sample_type,
        }
    }

    /// A deliberately independent protobuf reader: it walks tag/length pairs
    /// without any knowledge of the OTLP schema, so it cannot agree with a
    /// field-number mistake in the encoder the way a mirrored decoder would.
    struct Scanner<'a> {
        buf: &'a [u8],
        pos: usize,
    }

    impl<'a> Scanner<'a> {
        fn new(buf: &'a [u8]) -> Self {
            Self { buf, pos: 0 }
        }
        fn varint(&mut self) -> u64 {
            let mut out = 0u64;
            let mut shift = 0;
            loop {
                let b = self.buf[self.pos];
                self.pos += 1;
                out |= ((b & 0x7f) as u64) << shift;
                if b & 0x80 == 0 {
                    return out;
                }
                shift += 7;
            }
        }
        /// Next `(field_number, wire_type, payload)`; payload is the raw
        /// bytes for wire type 2 and the fixed 8 bytes for wire type 1.
        fn next(&mut self) -> Option<(u32, u32, &'a [u8])> {
            if self.pos >= self.buf.len() {
                return None;
            }
            let key = self.varint();
            let (field, wire) = ((key >> 3) as u32, (key & 7) as u32);
            match wire {
                0 => {
                    let start = self.pos;
                    self.varint();
                    Some((field, wire, &self.buf[start..self.pos]))
                }
                1 => {
                    let s = self.pos;
                    self.pos += 8;
                    Some((field, wire, &self.buf[s..self.pos]))
                }
                2 => {
                    let len = self.varint() as usize;
                    let s = self.pos;
                    self.pos += len;
                    Some((field, wire, &self.buf[s..self.pos]))
                }
                other => panic!("unexpected wire type {other}"),
            }
        }
        /// The single occurrence of `field`, panicking if absent.
        fn field(buf: &'a [u8], field: u32) -> &'a [u8] {
            let mut sc = Scanner::new(buf);
            while let Some((f, _, payload)) = sc.next() {
                if f == field {
                    return payload;
                }
            }
            panic!("field {field} not present");
        }
        fn all(buf: &'a [u8], field: u32) -> Vec<&'a [u8]> {
            let mut sc = Scanner::new(buf);
            let mut out = Vec::new();
            while let Some((f, _, payload)) = sc.next() {
                if f == field {
                    out.push(payload);
                }
            }
            out
        }
    }

    /// The wire is protobuf, not JSON, and every field number/wire type
    /// matches `opentelemetry-proto` v1.
    ///
    /// FAILS ON PRE-FIX CODE: `build_export_request_protobuf` did not exist;
    /// the only encoder produced `serde_json` text, whose first byte is `{`
    /// (0x7B) and which decodes as field 15 wire type 3 — an immediate panic
    /// in `Scanner`.
    #[test]
    fn wire_field_numbers_match_otlp_schema() {
        let mut metrics: HashMap<String, Vec<Sample>> = HashMap::new();
        metrics.insert(
            "http_req_duration".into(),
            vec![sample_at(
                "http_req_duration",
                12.5,
                SampleType::Trend,
                &[("status", "200")],
                1_700_000_000_000_000_000,
            )],
        );
        let bytes = build_export_request_protobuf(&metrics);

        // It is protobuf, not JSON.
        assert_ne!(bytes[0], b'{', "still emitting JSON, not protobuf");

        // ExportMetricsServiceRequest.resource_metrics = 1
        let rm = Scanner::field(&bytes, 1);
        // ResourceMetrics.resource = 1 → Resource.attributes = 1 → KeyValue
        let resource = Scanner::field(rm, 1);
        let kv = Scanner::field(resource, 1);
        assert_eq!(Scanner::field(kv, 1), b"service.name"); // KeyValue.key
        let any = Scanner::field(kv, 2); // KeyValue.value: AnyValue
        assert_eq!(Scanner::field(any, 1), b"tropel"); // AnyValue.string_value

        // ResourceMetrics.scope_metrics = 2
        let sm = Scanner::field(rm, 2);
        let scope = Scanner::field(sm, 1); // ScopeMetrics.scope = 1
        assert_eq!(Scanner::field(scope, 1), b"tropel"); // InstrumentationScope.name

        // ScopeMetrics.metrics = 2
        let metric = Scanner::field(sm, 2);
        assert_eq!(Scanner::field(metric, 1), b"http_req_duration"); // Metric.name

        // Trend → Metric.gauge = 5 (NOT sum = 7).
        let gauge = Scanner::field(metric, 5);
        assert!(
            Scanner::all(metric, 7).is_empty(),
            "a Trend must not ship as a Sum"
        );

        // Gauge.data_points = 1
        let dp = Scanner::field(gauge, 1);
        // NumberDataPoint.time_unix_nano = 3, fixed64 little-endian.
        let mut sc = Scanner::new(dp);
        let mut saw_time = false;
        let mut saw_double = false;
        while let Some((f, wire, payload)) = sc.next() {
            match f {
                3 => {
                    assert_eq!(wire, 1, "time_unix_nano must be fixed64, not varint");
                    assert_eq!(
                        u64::from_le_bytes(payload.try_into().unwrap()),
                        1_700_000_000_000_000_000
                    );
                    saw_time = true;
                }
                4 => {
                    assert_eq!(wire, 1, "as_double must be a double, not a varint");
                    assert_eq!(f64::from_bits(u64::from_le_bytes(payload.try_into().unwrap())), 12.5);
                    saw_double = true;
                }
                7 => {
                    assert_eq!(Scanner::field(payload, 1), b"status");
                    assert_eq!(Scanner::field(Scanner::field(payload, 2), 1), b"200");
                }
                2 => panic!("a Gauge point must not carry start_time_unix_nano"),
                other => panic!("unexpected NumberDataPoint field {other}"),
            }
        }
        assert!(saw_time && saw_double);
    }

    /// Counters aggregate per tag-set and ship as a monotonic DELTA `Sum`
    /// carrying `start_time_unix_nano` — the invariant PR #398 fixed on the
    /// JSON path, re-asserted on the protobuf wire.
    ///
    /// FAILS ON PRE-FIX CODE: there was no protobuf encoder to carry it.
    #[test]
    fn counter_is_delta_sum_with_start_time() {
        let mut metrics: HashMap<String, Vec<Sample>> = HashMap::new();
        metrics.insert(
            "http_reqs".into(),
            vec![
                sample_at("http_reqs", 1.0, SampleType::Counter, &[("status", "200")], 100),
                sample_at("http_reqs", 1.0, SampleType::Counter, &[("status", "200")], 300),
                sample_at("http_reqs", 1.0, SampleType::Counter, &[("status", "500")], 200),
            ],
        );
        let bytes = build_export_request_protobuf(&metrics);

        let metric = Scanner::field(
            Scanner::field(Scanner::field(&bytes, 1), 2), // resource_metrics → scope_metrics
            2,                                            // metrics
        );
        let sum = Scanner::field(metric, 7); // oneof data: sum = 7
        assert!(
            Scanner::all(metric, 5).is_empty(),
            "a Counter must not ship as a Gauge"
        );

        // Sum.aggregation_temporality = 2 → 1 (DELTA), Sum.is_monotonic = 3 → true.
        let temporality = Scanner::field(sum, 2);
        assert_eq!(Scanner::new(temporality).varint(), 1, "must be DELTA (1)");
        let monotonic = Scanner::field(sum, 3);
        assert_eq!(Scanner::new(monotonic).varint(), 1, "must be monotonic");

        // Two data points: status=200 summed to 2.0, status=500 to 1.0, each
        // with start_time_unix_nano = the earliest sample in the window.
        let points = Scanner::all(sum, 1);
        assert_eq!(points.len(), 2, "one aggregated delta point per tag-set");
        let mut seen: Vec<(String, f64, u64)> = Vec::new();
        for dp in points {
            let status =
                String::from_utf8(Scanner::field(Scanner::field(Scanner::field(dp, 7), 2), 1).to_vec())
                    .unwrap();
            let value = f64::from_bits(u64::from_le_bytes(
                Scanner::field(dp, 4).try_into().unwrap(),
            ));
            let start = u64::from_le_bytes(Scanner::field(dp, 2).try_into().unwrap());
            seen.push((status, value, start));
        }
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(seen[0].0, "200");
        assert_eq!(seen[0].1, 2.0, "two events at status=200 must sum to 2.0");
        assert_eq!(seen[1].0, "500");
        assert_eq!(seen[1].1, 1.0);
        assert_eq!(seen[0].2, 100, "start_time must be the window's earliest");
        assert_eq!(seen[1].2, 100);
    }

    /// Exact wire bytes for the smallest possible request, hand-derived from
    /// the protobuf spec. This is the check that catches a field number that
    /// is wrong in the encoder AND wrong in the same way in a reader.
    ///
    /// Derivation (a Gauge, one point, no attributes, t = 1 ns, v = 1.0):
    ///
    /// ```text
    /// NumberDataPoint      : `19` + fixed64 t=1                     9 B
    ///                        `21` + double 1.0                      9 B
    ///   → payload 18 B, framed `0a 12` inside Gauge                20 B
    /// Gauge                : payload 20 B, framed `2a 14`          22 B
    /// Metric               : `0a 01 61` (name "a")  3 B + 22 B  =  25 B
    ///   → framed `12 19` inside ScopeMetrics                       27 B
    /// InstrumentationScope : `0a 06 tropel`                         8 B
    ///   → framed `0a 08` inside ScopeMetrics                       10 B
    /// ScopeMetrics         : payload 10 + 27 = 37 B, framed `12 25`
    ///                                                              39 B
    /// KeyValue             : `0a 0c service.name`  14 B
    ///                        `12 08 0a 06 tropel`  10 B         =  24 B
    /// Resource             : framed `0a 18`                        26 B
    ///   → framed `0a 1a` inside ResourceMetrics                    28 B
    /// ResourceMetrics      : payload 28 + 39 = 67 B, framed `0a 43`
    /// ```
    #[test]
    fn golden_wire_bytes() {
        let mut metrics: HashMap<String, Vec<Sample>> = HashMap::new();
        metrics.insert(
            "a".into(),
            vec![sample_at("a", 1.0, SampleType::Trend, &[], 1)],
        );
        let bytes = build_export_request_protobuf(&metrics);

        let mut expect: Vec<u8> = Vec::new();
        // ExportMetricsServiceRequest.resource_metrics (field 1, len 67)
        expect.extend_from_slice(&[0x0a, 67]);
        //   ResourceMetrics.resource (field 1, len 26)
        expect.extend_from_slice(&[0x0a, 26]);
        //     Resource.attributes (field 1, len 24)
        expect.extend_from_slice(&[0x0a, 24]);
        //       KeyValue.key = "service.name"
        expect.extend_from_slice(&[0x0a, 12]);
        expect.extend_from_slice(b"service.name");
        //       KeyValue.value = AnyValue{string_value = "tropel"}
        expect.extend_from_slice(&[0x12, 8, 0x0a, 6]);
        expect.extend_from_slice(b"tropel");
        //   ResourceMetrics.scope_metrics (field 2, len 37)
        expect.extend_from_slice(&[0x12, 37]);
        //     ScopeMetrics.scope (field 1, len 8)
        expect.extend_from_slice(&[0x0a, 8, 0x0a, 6]);
        expect.extend_from_slice(b"tropel");
        //     ScopeMetrics.metrics (field 2, len 25)
        expect.extend_from_slice(&[0x12, 25]);
        //       Metric.name = "a"
        expect.extend_from_slice(&[0x0a, 1, b'a']);
        //       Metric.gauge (field 5, len 20)
        expect.extend_from_slice(&[0x2a, 20]);
        //         Gauge.data_points (field 1, len 18)
        expect.extend_from_slice(&[0x0a, 18]);
        //           time_unix_nano (field 3, fixed64) = 1
        expect.extend_from_slice(&[0x19]);
        expect.extend_from_slice(&1u64.to_le_bytes());
        //           as_double (field 4, double) = 1.0
        expect.extend_from_slice(&[0x21]);
        expect.extend_from_slice(&1.0f64.to_bits().to_le_bytes());

        assert_eq!(
            hex(&bytes),
            hex(&expect),
            "wire bytes drifted from the hand-derived OTLP encoding"
        );
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Third-party check: `prost` (a dev-dependency, not linked into the
    /// binary) decodes our bytes with message definitions declared here.
    /// This catches wire-type and length-framing errors that a schema-blind
    /// scanner tolerates — `prost` rejects a malformed length prefix.
    ///
    /// FAILS ON PRE-FIX CODE: JSON text does not decode as protobuf.
    #[test]
    fn round_trip_decodes_under_prost() {
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct AnyValue {
            #[prost(string, optional, tag = "1")]
            string_value: Option<String>,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct KeyValue {
            #[prost(string, tag = "1")]
            key: String,
            #[prost(message, optional, tag = "2")]
            value: Option<AnyValue>,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct NumberDataPoint {
            #[prost(fixed64, tag = "2")]
            start_time_unix_nano: u64,
            #[prost(fixed64, tag = "3")]
            time_unix_nano: u64,
            #[prost(double, tag = "4")]
            as_double: f64,
            #[prost(message, repeated, tag = "7")]
            attributes: Vec<KeyValue>,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct Gauge {
            #[prost(message, repeated, tag = "1")]
            data_points: Vec<NumberDataPoint>,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct Sum {
            #[prost(message, repeated, tag = "1")]
            data_points: Vec<NumberDataPoint>,
            #[prost(int32, tag = "2")]
            aggregation_temporality: i32,
            #[prost(bool, tag = "3")]
            is_monotonic: bool,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct Metric {
            #[prost(string, tag = "1")]
            name: String,
            #[prost(message, optional, tag = "5")]
            gauge: Option<Gauge>,
            #[prost(message, optional, tag = "7")]
            sum: Option<Sum>,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct InstrumentationScope {
            #[prost(string, tag = "1")]
            name: String,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct ScopeMetrics {
            #[prost(message, optional, tag = "1")]
            scope: Option<InstrumentationScope>,
            #[prost(message, repeated, tag = "2")]
            metrics: Vec<Metric>,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct Resource {
            #[prost(message, repeated, tag = "1")]
            attributes: Vec<KeyValue>,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct ResourceMetrics {
            #[prost(message, optional, tag = "1")]
            resource: Option<Resource>,
            #[prost(message, repeated, tag = "2")]
            scope_metrics: Vec<ScopeMetrics>,
        }
        #[derive(Clone, PartialEq, ::prost::Message)]
        struct ExportMetricsServiceRequest {
            #[prost(message, repeated, tag = "1")]
            resource_metrics: Vec<ResourceMetrics>,
        }

        let mut metrics: HashMap<String, Vec<Sample>> = HashMap::new();
        metrics.insert(
            "http_reqs".into(),
            vec![
                sample_at("http_reqs", 1.0, SampleType::Counter, &[("status", "200")], 10),
                sample_at("http_reqs", 3.0, SampleType::Counter, &[("status", "200")], 20),
            ],
        );
        metrics.insert(
            "http_req_duration".into(),
            vec![sample_at(
                "http_req_duration",
                42.25,
                SampleType::Trend,
                &[("method", "GET"), ("url", "https://example.test/a")],
                30,
            )],
        );

        let bytes = build_export_request_protobuf(&metrics);
        let decoded = <ExportMetricsServiceRequest as prost::Message>::decode(bytes.as_slice())
            .expect("prost must accept the encoded request");

        assert_eq!(decoded.resource_metrics.len(), 1);
        let rm = &decoded.resource_metrics[0];
        let res = rm.resource.as_ref().unwrap();
        assert_eq!(res.attributes[0].key, "service.name");
        assert_eq!(
            res.attributes[0].value.as_ref().unwrap().string_value.as_deref(),
            Some("tropel")
        );
        assert_eq!(rm.scope_metrics.len(), 1);
        let sm = &rm.scope_metrics[0];
        assert_eq!(sm.scope.as_ref().unwrap().name, "tropel");
        assert_eq!(sm.metrics.len(), 2);

        let counter = sm.metrics.iter().find(|m| m.name == "http_reqs").unwrap();
        let sum = counter.sum.as_ref().expect("Counter must decode as a Sum");
        assert!(counter.gauge.is_none());
        assert_eq!(sum.aggregation_temporality, 1, "DELTA");
        assert!(sum.is_monotonic);
        assert_eq!(sum.data_points.len(), 1, "one tag-set → one delta point");
        assert_eq!(sum.data_points[0].as_double, 4.0, "1.0 + 3.0");
        assert_eq!(sum.data_points[0].start_time_unix_nano, 10);
        assert_eq!(sum.data_points[0].time_unix_nano, 20, "newest in window");

        let trend = sm
            .metrics
            .iter()
            .find(|m| m.name == "http_req_duration")
            .unwrap();
        let gauge = trend.gauge.as_ref().expect("Trend must decode as a Gauge");
        assert!(trend.sum.is_none());
        assert_eq!(gauge.data_points.len(), 1);
        assert_eq!(gauge.data_points[0].as_double, 42.25);
        assert_eq!(gauge.data_points[0].time_unix_nano, 30);
        assert_eq!(
            gauge.data_points[0].start_time_unix_nano, 0,
            "a Gauge carries no start time"
        );
        let mut attrs: Vec<(String, String)> = gauge.data_points[0]
            .attributes
            .iter()
            .map(|kv| {
                (
                    kv.key.clone(),
                    kv.value
                        .as_ref()
                        .unwrap()
                        .string_value
                        .clone()
                        .unwrap_or_default(),
                )
            })
            .collect();
        attrs.sort();
        assert_eq!(
            attrs,
            vec![
                ("method".to_string(), "GET".to_string()),
                ("url".to_string(), "https://example.test/a".to_string()),
            ]
        );
    }

    /// A tag value longer than 127 bytes forces a two-byte length varint at
    /// every nesting level above it. This is the case where an encoder that
    /// reserves one byte for the length prefix corrupts the stream.
    ///
    /// FAILS ON PRE-FIX CODE: no protobuf encoder existed to get this wrong.
    #[test]
    fn multi_byte_length_prefixes_are_framed_correctly() {
        let long_url = format!("https://example.test/{}", "p".repeat(400));
        let mut metrics: HashMap<String, Vec<Sample>> = HashMap::new();
        metrics.insert(
            "http_req_duration".into(),
            vec![sample_at(
                "http_req_duration",
                1.0,
                SampleType::Trend,
                &[("url", long_url.as_str())],
                7,
            )],
        );
        let bytes = build_export_request_protobuf(&metrics);

        // Every nesting level must re-parse cleanly with the schema-blind
        // scanner; a wrong length prefix runs the cursor off the end.
        let dp = Scanner::field(
            Scanner::field(
                Scanner::field(Scanner::field(Scanner::field(&bytes, 1), 2), 2),
                5,
            ),
            1,
        );
        let attr = Scanner::field(dp, 7);
        assert_eq!(Scanner::field(attr, 1), b"url");
        assert_eq!(
            Scanner::field(Scanner::field(attr, 2), 1),
            long_url.as_bytes()
        );
    }

    #[test]
    fn varint_len_matches_encoded_width() {
        // The length arithmetic is only correct if `varint_len` agrees with
        // what `put_varint` actually writes — an off-by-one here silently
        // desynchronises every nested length prefix.
        for v in [
            0u64,
            1,
            127,
            128,
            16_383,
            16_384,
            2_097_151,
            2_097_152,
            268_435_455,
            268_435_456,
            u32::MAX as u64,
            u64::MAX / 2,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(varint_len(v), buf.len(), "varint_len({v}) disagrees");
        }
    }

    /// An empty flush window still produces a well-formed (if empty) request
    /// rather than a truncated one.
    #[test]
    fn empty_metrics_still_frames() {
        let metrics: HashMap<String, Vec<Sample>> = HashMap::new();
        let bytes = build_export_request_protobuf(&metrics);
        let sm = Scanner::field(Scanner::field(&bytes, 1), 2);
        assert!(Scanner::all(sm, 2).is_empty(), "no metrics");
        assert_eq!(Scanner::field(Scanner::field(sm, 1), 1), b"tropel");
    }
}
