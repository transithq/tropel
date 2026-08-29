# Insomnia fixtures

**These must be workspaces exported by Insomnia, and at least one must be
exported AFTER dragging items to reorder them.**

TR-006's acceptance criterion says so, and names the reason: `metaSortKey` was
typed `Option<i64>`, while Insomnia assigns sort keys by **midpoint averaging**
— they go fractional the first time a user drags anything, and the whole
export then fails to deserialize. A hand-written fixture with integer sort
keys can never catch that; only a genuinely reordered export can.

Adding a fixture here was also blocked until TR-008: the root `.gitignore`
carried a blanket `*.json`, so any fixture dropped in this directory was
silently untracked.

When adding one: export from Insomnia, commit it verbatim, do not tidy it.
