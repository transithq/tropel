#!/usr/bin/env bash
# Every include_str!/include_bytes!/include! path must resolve INSIDE its own
# crate directory.
#
# This exists because tropel-input-k6 0.6.0 was published BROKEN and cargo
# said it was fine. Its sources embedded the repo-root js/ tree via
# `include_str!("../../../../js/...")`. Cargo cannot package files from
# outside a package root, so the tarball shipped without them — but the
# verify build passed, because cargo verifies inside `target/package/`, which
# is INSIDE the repo: from `target/package/<crate>/src/`, four `..` climb back
# out to the real repo root and find the real js/ tree. The deeper a crate
# sits under crates/, the more likely its own escape path lands somewhere
# real. tropel-web, one level shallower, failed loudly; tropel-input-k6
# published silently and nobody could build it.
#
# So a green `cargo publish --dry-run`, verify build included, does NOT prove
# a tarball is self-contained. This check does, statically and in a second:
# a path that does not escape the crate root cannot resolve to a file the
# tarball lacks.
#
# Paths are normalised LEXICALLY, not with realpath — `crates/x/src/../js/a.js`
# must read as `crates/x/js/a.js`. The js/ entry in those crates is a symlink
# to the single tree at the repo root, which cargo dereferences into the
# tarball; resolving symlinks here would report the real target outside the
# crate and defeat the check.
set -uo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import os, re, sys

pat = re.compile(r'include(?:_str|_bytes)?!\s*\(\s*"([^"]+)"\s*\)')
roots = []
for base in ('crates',):
    for dirpath, dirnames, filenames in os.walk(base):
        if 'target' in dirpath.split(os.sep):
            continue
        if 'Cargo.toml' in filenames:
            roots.append(dirpath)
            dirnames[:] = [d for d in dirnames if d != 'target']

bad = []
checked = 0
for root in sorted(roots):
    for sub in ('src', 'tests', 'benches', 'examples'):
        for dirpath, _, filenames in os.walk(os.path.join(root, sub)):
            for fn in filenames:
                if not fn.endswith('.rs'):
                    continue
                f = os.path.join(dirpath, fn)
                try:
                    src = open(f, encoding='utf-8', errors='replace').read()
                except OSError:
                    continue
                for m in pat.finditer(src):
                    p = m.group(1)
                    checked += 1
                    # lexical, NOT realpath — see the header
                    resolved = os.path.normpath(os.path.join(os.path.dirname(f), p))
                    if os.path.relpath(resolved, root).startswith('..'):
                        line = src[:m.start()].count('\n') + 1
                        bad.append((f, line, p, root))

print(f"── include! paths confined to their crate ──")
print(f"{checked} literal include paths checked across {len(roots)} crates")
if bad:
    print(f"\n{len(bad)} ESCAPE their crate root — these package into a tarball")
    print("that cannot build, even if `cargo publish` verifies locally:\n")
    for f, line, p, root in bad:
        print(f"  {f}:{line}")
        print(f"      {p}   escapes {root}/")
    sys.exit(1)
print("ok: none escape")
PY
