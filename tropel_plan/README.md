# Tropel — Implementation Roadmap

A task-by-task build plan for `transithq/tropel`, written to be executed by AI agents that open and merge PRs.

## Read these first, in order

| File | Why |
|---|---|
| **`CONTEXT.md`** | What tropel is, the three layers of problem, the settled decisions, and the invariants you must not break. **Every agent reads this before touching code.** |
| **`CONVENTIONS.md`** | Branch/commit/PR format, evidence grades, definition of done, what needs human sign-off. |
| **`ROADMAP.md`** | Wave order, the dependency graph, and what can run in parallel. |
| **`VERIFICATION.md`** | This plan audited against `master` @ `2099cbe`. **Read it before starting any task** — the tree was reorganized, so most register line numbers no longer resolve, and seven items are already closed. |
| `tasks/*.md` | The tasks themselves. |

## Task ID scheme

`TR-<wave><nn>` — e.g. `TR-004` is wave 0 task 4, `TR-412` is wave 4 task 12. IDs are stable; never renumber. Knockport's tasks are `KP-*`; the two schemes never collide, and a task that needs both sides names both ids.

## Relationship to the existing documents

**`TROPEL_MASTER_TODO.md` remains the evidence register and the single source of truth for findings.** It carries the line numbers, the reproduction, and the evidence grade for all 253 open items. This folder does not restate that evidence — it is the *execution* form: waves, gates, dependencies, acceptance criteria, and tests.

Every task cites its source section. When they disagree, the register wins on facts and this folder wins on ordering. Closing a task means ticking it **in the register too**, or the next round re-files it.

| Source | Carries |
|---|---|
| `TROPEL_MASTER_TODO.md` | The defect register — 253 open, 48 closed, evidence-graded. Sections `W0`–`W6`, `P-0`–`P-K`, `W-R4` |
| `TROPEL_PARITY_K6.md` | k6 v2.1.0 coverage, read first-hand from `grafana/k6@53b5727`. Feeds **W2** |
| `TROPEL_PARITY_POSTMAN.md` | Postman/Newman coverage. Feeds **W2 Track D** |
| `TROPEL_PERF_VS_K6.md` | The measured throughput gap and its cause. Feeds **W3**, **W5** |
| `TROPEL_EXEC_SPLIT.md` | How the three artifacts ship, and the SDK inversion. Feeds **W4**, **W6** |
| `knockport/tasks/*.md` | The consumer. Anything it needs from this repo is **W4** |

## Task anatomy

Every task carries: **Problem** (with evidence), **Approach** where the fix is non-obvious, **Acceptance criteria**, **Tests required**, and **Blocked by**. If a task lacks enough detail to implement, say so in the PR rather than guessing.
