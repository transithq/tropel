#!/usr/bin/env bash
# Generate the GitHub release body for a given version.
#
#   bash scripts/release-notes.sh 0.2.0 > RELEASE_NOTES.md
#
# WHY THIS EXISTS: release.yml used `body_path: CHANGELOG.md`, which pastes the
# ENTIRE changelog — every historical version — into every release page. That
# buries the release being announced, and it shipped withdrawn measurements
# from old entries as if they were current.
#
# This emits what an open-source release page is actually for: what the thing
# is, how to install it on each platform, how to verify the download, what is
# inside each archive, and the highlights for THIS version — then links to the
# full changelog rather than inlining it.
#
# The highlights come from CHANGELOG.md's own `## [x.y.z]` section, so the
# release notes cannot drift from the changelog. A missing section is a hard
# error, not a silently empty release body.
set -euo pipefail
cd "$(dirname "$0")/.."

V="${1:?usage: release-notes.sh <version>   e.g. 0.2.0}"

# Extract this version's section: from `## [V]` up to (not including) the next
# `## ` heading. awk rather than sed so the terminator is unambiguous.
SECTION=$(awk -v ver="$V" '
  $0 ~ "^## \\[" ver "\\]" { inside = 1; next }
  inside && /^## / { exit }
  inside { print }
' CHANGELOG.md)

if [[ -z "$(tr -d '[:space:]' <<<"$SECTION")" ]]; then
  echo "FAIL: CHANGELOG.md has no '## [$V]' section — write it before tagging." >&2
  exit 1
fi

cat <<EOF
**Tropel** is an open-source load-testing framework in Rust. It runs
**Postman collections**, **HAR files**, **OpenAPI specs**, **Bruno** and
**Insomnia** exports, and **k6 scripts** as load tests — a native Rust hot
path with an embedded QuickJS engine for script execution.

> **Pre-1.0: the API is unstable.** k6 parity is actively expanding and the
> \`tropel-sdk\` surface still changes between minor versions. Pin an exact
> version if you depend on it.

## Install

Pick your platform. Linux builds are **static musl** — they run unmodified in
Alpine, distroless and scratch containers.

\`\`\`bash
# Linux x86_64
curl -fsSL https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo install -m755 tropel-v$V-x86_64-unknown-linux-musl/tropel /usr/local/bin/

# Linux arm64
curl -fsSL https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-aarch64-unknown-linux-musl.tar.gz | tar xz
sudo install -m755 tropel-v$V-aarch64-unknown-linux-musl/tropel /usr/local/bin/

# macOS Apple Silicon
curl -fsSL https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-aarch64-apple-darwin.tar.gz | tar xz
sudo install -m755 tropel-v$V-aarch64-apple-darwin/tropel /usr/local/bin/

# macOS Intel
curl -fsSL https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-x86_64-apple-darwin.tar.gz | tar xz
sudo install -m755 tropel-v$V-x86_64-apple-darwin/tropel /usr/local/bin/
\`\`\`

Windows (PowerShell):

\`\`\`powershell
Invoke-WebRequest -Uri "https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-x86_64-pc-windows-msvc.zip" -OutFile tropel.zip
Expand-Archive tropel.zip -DestinationPath .
\`\`\`

macOS note: binaries are unsigned, so Gatekeeper will quarantine a downloaded
archive. Clear it with \`xattr -d com.apple.quarantine ./tropel\`.

### Verify your download

\`\`\`bash
curl -fsSLO https://github.com/transithq/tropel/releases/download/v$V/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing      # macOS: shasum -a 256 -c
\`\`\`

### From source (Rust 1.94+)

\`\`\`bash
cargo build --release      # ./target/release/tropel
\`\`\`

## Assets

| asset | contents |
|---|---|
| \`tropel-v$V-<target>.tar.gz\` / \`.zip\` | \`tropel\`, \`tropel-controller\`, \`tropel-agent\` + LICENSE, README |
| \`tropel-wasm-v$V.tar.gz\` | the browser/edge tier — \`core-wasm\`, \`input-wasm\`, \`runtime-wasm\`, \`shims\` |
| \`SHA256SUMS\` | checksums over every asset above |

Targets: \`x86_64\`/\`aarch64\` Linux (musl), \`x86_64\`/\`aarch64\` macOS,
\`x86_64\` Windows (MSVC).

## Quick start

\`\`\`bash
tropel run collection.json --vus 50 --duration 2m   # Postman, HAR, OpenAPI, k6, Bruno, Insomnia
tropel inspect collection.json                      # preview without sending traffic
tropel extensions                                   # list the input formats this binary ships
\`\`\`

Full docs: [docs/](https://github.com/transithq/tropel/tree/v$V/docs) — CLI
reference, executors, scripting, metrics, extensions, distributed execution.

## What's in it

- **Seven executors** — constant-vus, ramping-vus, shared/per-vu iterations,
  constant & ramping arrival rate, externally-controlled, with graceful
  stop/ramp-down, think time and pacing
- **Postman \`pm.*\` scripting** — tests, assertions, variable scopes,
  \`setNextRequest\`, \`sendRequest\`, custom metrics
- **k6-style scripting** — \`http.*\`, \`check()\`, \`group()\`, \`sleep()\`,
  \`Counter\`/\`Gauge\`/\`Rate\`/\`Trend\`; exported \`options\` honoured
- **HDR-histogram metrics** — p50/p90/p95/p99, sub-timings, tag-scoped
  aggregation, thresholds with k6-compatible abort semantics
- **Streaming outputs** — NDJSON, StatsD, InfluxDB, Prometheus, OTLP
  (protobuf + gzip), plus stdout/JSON/CSV reporters
- **Distributed** — k6-style execution segments for multi-node runs
- **Extensible** — \`tropel-sdk\` + \`tropel build --with <crate>\`, plus a
  sandboxed WASM plugin tier for third-party input formats

## npm packages

\`\`\`
@tropel/core-wasm@$V   @tropel/input-wasm@$V   @tropel/runtime-wasm@$V   @tropel/shims@$V
\`\`\`

## Changes in $V
$SECTION
---

Full changelog: [CHANGELOG.md](https://github.com/transithq/tropel/blob/v$V/CHANGELOG.md) ·
Issues: [github.com/transithq/tropel/issues](https://github.com/transithq/tropel/issues) ·
Licence: Apache-2.0
EOF
