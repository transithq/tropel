# Bruno fixtures

**These must be files produced by Bruno's own exporter, not hand-written JSON.**

TR-005's acceptance criterion says so explicitly, and names the reason: the
adapter matched only `type: "http-request"`, while Bruno's exporter rewrites
that to `"http"` (`transformItem` in `bruno-app/src/utils/collections/export.js`).
Every test used inline JSON that spelled it the internal way, so **no real
export would have parsed** and the whole suite was green.

Adding a fixture here was also blocked until TR-008: the root `.gitignore`
carried a blanket `*.json`, so any fixture dropped in this directory was
silently untracked.

When adding one: export from Bruno, commit it verbatim, do not tidy it.
