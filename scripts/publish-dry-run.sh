#!/usr/bin/env bash
# TR-601: `cargo publish --dry-run` for every publishable crate.
#
# The 0.1.0 release gate carries the line "cargo publish --dry-run succeeds
# for every publishable crate", and it was ticked while four of the seven
# failed. This script is the check, so the claim can be re-derived in one
# command instead of asserted.
#
# The blocker is structural, not a bug: a `path` + `version` dependency only
# resolves at publish time if that EXACT version is already on crates.io.
# The workspace pins `tropel-sdk = { path = ..., version = "0.3.0" }` and
# crates.io has 0.1.0 and 0.2.0, so every dependent fails until tropel-sdk
# 0.3.0 is published. Publishing needs a human (CONVENTIONS: "Versions are
# permanent"), which is why this reports rather than blocks.
set -uo pipefail
cd "$(dirname "$0")/.."

PUBLISHABLE=()
while IFS= read -r manifest; do
  # `publish = false` opts out; anything else is publishable.
  if grep -qE '^publish\s*=\s*false' "$manifest"; then continue; fi
  name=$(grep -m1 '^name' "$manifest" | sed -E 's/name *= *"([^"]+)"/\1/')
  [[ -n "$name" ]] && PUBLISHABLE+=("$name")
done < <(find crates -name Cargo.toml -maxdepth 3 | sort)

echo "── TR-601: cargo publish --dry-run ──"
failed=()
for crate in "${PUBLISHABLE[@]}"; do
  printf '%-22s ' "$crate"
  if out=$(cargo publish --dry-run -p "$crate" --allow-dirty --no-verify 2>&1); then
    echo "ok"
  else
    echo "FAIL"
    echo "$out" | sed -n '/^error/,+4p' | sed 's/^/    /'
    failed+=("$crate")
  fi
done

if [[ ${#failed[@]} -gt 0 ]]; then
  echo
  echo "BLOCKED: ${#failed[@]} of ${#PUBLISHABLE[@]} publishable crates cannot be published: ${failed[*]}" >&2
  echo "The 0.1.0 release gate is NOT met. See TR-601." >&2
  # Exit 0 by default: this reports a release-readiness fact, and wedging
  # every PR on it helps nobody. Set TROPEL_PUBLISH_GATE=1 (the release job)
  # to make it blocking.
  [[ "${TROPEL_PUBLISH_GATE:-0}" == "1" ]] && exit 1
  exit 0
fi
echo
echo "ok: every publishable crate passes --dry-run — the TR-601 gate is met"
