# Outputs & Reporters

## Reporters (end-of-run)

| Reporter | Flags |
|----------|-------|
| stdout (default) | `-r stdout` |
| JSON | `-r json -o results.json` |
| CSV | `-r csv -o results.csv` |

## Streaming outputs (during the run)

An `Output` trait consumes an MPSC stream of samples on a dedicated task, so
metrics flow out as the test runs, not just at the end:

| Output | Flag |
|--------|------|
| NDJSON stream (k6 `--out json=`) | `--json-stream <file>` |
| StatsD / Datadog UDP | `--statsd-addr <host:port>` |
| InfluxDB line protocol UDP | `--influxdb-addr <host:port>` |
| Prometheus remote-write | `--prometheus-url <url>` |
| OTLP/HTTP | `--otlp-endpoint <url>` |
| k6-style summary object | `--summary-export <file>` |

### OTLP/HTTP wire encoding

OTLP exports ship **gzipped protobuf** (`Content-Type: application/x-protobuf`,
`Content-Encoding: gzip`). Protobuf is the encoding the OTLP specification
requires every OTLP/HTTP receiver to support — OTLP/JSON is optional there — so
it is both the more interoperable choice and dramatically cheaper to produce.

For a receiver that only speaks OTLP/JSON, set:

```bash
TROPEL_OTLP_PROTOCOL=json tropel run collection.json --otlp-endpoint http://localhost:4318
```

Accepted values are `protobuf` (aliases `proto`, `pb`, `http/protobuf`) and
`json` (alias `http/json`). An unrecognised value warns and falls back to
protobuf rather than failing the run. Both encodings carry identical
semantics — same DELTA `Sum` aggregation, same `start_time_unix_nano`, same
resource and scope attributes.

## Per-request debugging

```bash
tropel run collection.json --http-debug
```

Logs `HTTP >>>` (method/url/body bytes/header count) before each send and
`HTTP <<<` (status/bytes/duration) after — at info level, so no `RUST_LOG`
tuning is needed.
