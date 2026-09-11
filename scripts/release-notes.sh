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
# This emits what an open-source release page is actually for: what changed,
# and one command to get the thing. Everything else — the other four
# platforms, checksum verification, building from source, and the version's
# full changelog section — sits behind <details>, because a release page that
# opens with five curl blocks buries the one thing the reader came for.
#
# What tropel IS, its feature list and its quick start are deliberately NOT
# here. They are the README's job, and duplicating them meant every release
# page restated them and then drifted from them.
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

# Visible summary: the section's own lead paragraphs, then its `###` headings
# as bullets. The full section still ships, collapsed — a breaking-change note
# lives in a heading's BODY (0.6.0's tropel-auth signature change did), so
# headings alone would drop exactly what a reader must not miss.
LEAD=$(awk '/^### /{exit} {print}' <<<"$SECTION")
BULLETS=$(grep '^### ' <<<"$SECTION" | sed 's/^### /- **/; s/$/**/')

cat <<EOF
$LEAD

$BULLETS

<details>
<summary><b>Full notes for $V</b></summary>
$SECTION
</details>

## Install

Linux builds are **static musl** — they run unmodified in Alpine, distroless
and scratch containers.

\`\`\`bash
curl -fsSL https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo install -m755 tropel-v$V-x86_64-unknown-linux-musl/tropel /usr/local/bin/
\`\`\`

<details>
<summary><b>Other platforms, checksum verification, building from source</b></summary>

\`\`\`bash
# Linux arm64
curl -fsSL https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-aarch64-unknown-linux-musl.tar.gz | tar xz

# macOS Apple Silicon
curl -fsSL https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-aarch64-apple-darwin.tar.gz | tar xz

# macOS Intel
curl -fsSL https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-x86_64-apple-darwin.tar.gz | tar xz
\`\`\`

Windows (PowerShell):

\`\`\`powershell
Invoke-WebRequest -Uri "https://github.com/transithq/tropel/releases/download/v$V/tropel-v$V-x86_64-pc-windows-msvc.zip" -OutFile tropel.zip
Expand-Archive tropel.zip -DestinationPath .
\`\`\`

macOS binaries are unsigned, so Gatekeeper quarantines a downloaded archive.
Clear it with \`xattr -d com.apple.quarantine ./tropel\`.

\`\`\`bash
curl -fsSLO https://github.com/transithq/tropel/releases/download/v$V/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing      # macOS: shasum -a 256 -c
\`\`\`

From source (Rust 1.94+): \`cargo build --release\` puts it at
\`./target/release/tropel\`.

</details>

Each \`tropel-v$V-<target>\` archive carries \`tropel\`, \`tropel-controller\` and
\`tropel-agent\`; \`tropel-wasm-v$V.tar.gz\` is the browser tier
(\`core-wasm\`, \`input-wasm\`, \`runtime-wasm\`, \`shims\`, also on npm at
\`$V\`); \`SHA256SUMS\` covers every asset.

---

[Full changelog](https://github.com/transithq/tropel/blob/v$V/CHANGELOG.md) ·
[Docs](https://github.com/transithq/tropel/tree/v$V/docs) ·
[Issues](https://github.com/transithq/tropel/issues) · Apache-2.0
EOF
