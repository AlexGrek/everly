---
name: optimize
description: >-
  Find and fix performance problems in Everly (hot paths, locking, parallelism,
  allocations, hypertile-locality). Invoke for "/optimize", "optimize this",
  "make this faster", "find performance issues", or after implementing a feature
  that touches per-frame / per-entity / per-tile code. Always reads and updates
  OPTIMIZATION.md.
---

# optimize (Everly)

Drives a disciplined optimization pass: read the rules, find the real blockers,
fix the major ones, ask about the minor ones, and log every change so the
project's optimization history compounds instead of being re-derived.

## Step 0 — Read the rules (mandatory, first)

1. Read [`OPTIMIZATION.md`](../../../OPTIMIZATION.md) **in full** — both the
   "General rules" and the "Applied optimizations" log. Every fix in this pass
   must conform to those rules, and the log tells you what was already done (do
   not re-discover or re-apply a landed optimization).
2. If you will touch a specific subsystem, also read its skill/docs first
   (e.g. `actor-engineer`, `bevy-engineer`, `field-interactions`, the relevant
   `docs/*.md`). Optimizations must preserve documented behavior.

Do not skip Step 0. If `OPTIMIZATION.md` is missing, stop and say so — it is the
source of truth for this skill.

## Step 1 — Find the issues

Determine the scope:

- **If the user named a target**, optimize that.
- **Otherwise, default to "what was just implemented."** Inspect recent work with
  `git status` / `git diff` (and recent commits) and analyze those changed files.
- **If neither is clear**, ask the user what to optimize before proceeding.

Then hunt for performance problems, checking each changed/targeted area against
the `OPTIMIZATION.md` rules. Look specifically for:

- **Global locks on hot paths** — any `Mutex`/`RwLock`/global cache hit per
  entity, per cell, or per frame (rule 1).
- **Non-hypertile-local access** — per-cell chunk resolution, per-cell global
  table locks, `Arc` clones in a loop over one region (rule 2).
- **Large-value clones to read one field** — value-returning getters that copy a
  whole tile/struct (rule 3).
- **Per-frame / per-entity allocations** — `Vec`/`HashSet`/`HashMap`/`Box` built
  inside a per-actor or per-frame loop (rule 4).
- **Order-dependent or serial systems** that could be `par_iter_mut` if reads use
  an immutable snapshot and writes are commutative (rule 5).
- **Coarse locks held across multi-cell work** (rule 6).
- **Repeated validation / redundant probes / speculative writes to shared
  buffers** (rule 7).

Classify each finding as **major** (measurable per-frame cost, serializes
parallelism, allocates in the hot path, or scales badly with entity/tile count)
or **minor** (small constant, cold path, or stylistic).

Present the findings ranked, with `file:line` references and the rule each
violates, before changing anything.

## Step 2 — Do the work

- **Major issues: fix them now**, without asking. Each fix must be
  behavior-identical (rule 8): same outputs, same error variants, same edge
  cases. Prefer the smallest change that removes the blocker.
- **Minor issues: ask the user** whether to fix each (or batch them) — list them
  with the trade-off. Do not silently apply minor/risky changes.
- After each fix: `cargo check` (warning-clean) and run the touched test suites
  (e.g. `cargo test -p everly -- <area>`). Add or extend unit tests that pin the
  preserved behavior. Never bake a perf change without proving semantics hold.

Stay within the architecture conventions in `CLAUDE.md` (one subsystem = one
plugin, gate gameplay on `GameState::InGame`, no narrating comments, etc.).

## Step 3 — Update OPTIMIZATION.md (after each landed optimization)

This is required, not optional. After each major fix (or each batch of related
minor fixes) lands and verifies:

1. Append an entry to the **"Applied optimizations"** section of
   `OPTIMIZATION.md` — what was slow, what changed, the `file:line` references,
   and which rule(s) it serves. Group tightly-related fixes under one dated
   heading.
2. If the work surfaced a **new general rule** not yet captured, add it to the
   "General rules" section too.
3. If behavior-adjacent docs exist (e.g. `docs/actor.md`), update them per the
   owning subsystem's skill.

Do the update incrementally as you go, not only at the very end, so a partial
pass still leaves an accurate log.

## Finish

Summarize: findings (major fixed / minor pending-or-applied), verification
results (tests, `cargo check`), and the `OPTIMIZATION.md` entries you added.
Surface any minor issues still awaiting a user decision.

## Guardrails

- Read before code (Step 0). Never optimize hot-path/locking/parallelism code
  without having read `OPTIMIZATION.md`.
- Correctness first: a faster-but-different result is a regression, not an
  optimization.
- Don't add a new top-level dependency to chase performance; prefer what Bevy /
  std already provide.
- Don't micro-optimize cold paths or invent benchmarks the user didn't ask for —
  spend effort where it scales with entities, tiles, or frames.
