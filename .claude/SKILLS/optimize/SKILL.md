---
name: optimize
description: >-
  Find and fix performance problems in Everly (hot paths, locking, parallelism,
  allocations, hypertile-locality). Invoke for "/optimize", "optimize this",
  "make this faster", "find performance issues", or after implementing a feature
  that touches per-frame / per-entity / per-tile code. Always reads and updates
  OPTIMIZATION.md, and runs the full test suite between every step.
---

# optimize (Everly)

Drives a disciplined optimization pass: read the rules, establish a green
baseline, find the real blockers, fix the major ones, ask about the minor ones,
and log every change — re-running all tests between every step so a regression is
caught the moment it appears.

## Mandatory: run all tests between every step (even before the first)

Before Step 1 and after **every** subsequent step, run the full suite and a
warning-clean type-check:

```sh
cargo test -p everly        # all tests
cargo check -p everly       # must be warning-clean
```

Rules:

- **Baseline first.** Run the full suite *before touching anything*. If it is not
  green at baseline, stop and report — do not start optimizing on top of a broken
  tree (you could otherwise blame your change for a pre-existing failure).
- **Between each step.** Re-run after Step 0, after the analysis in Step 1, after
  every fix in Step 2, and after every `OPTIMIZATION.md` update in Step 3.
- **Red = halt.** If any test fails or a new warning appears, fix it before
  proceeding to the next step. Never advance with a red or warning-dirty tree.
- Record the pass/fail counts so the final summary can show baseline → final.

This is non-negotiable and applies on top of every step below.

## Step 0 — Read the rules (mandatory, first)

1. Read [`OPTIMIZATION.md`](../../../OPTIMIZATION.md) **in full** — both the
   "General rules" and the "Applied optimizations" log. Every fix in this pass
   must conform to those rules, and the log tells you what was already done (do
   not re-discover or re-apply a landed optimization).
2. If you will touch a specific subsystem, also read its skill/docs first
   (e.g. `actor-engineer`, `bevy-engineer`, `field-interactions`, the relevant
   `docs/*.md`). Optimizations must preserve documented behavior.
3. Run the baseline test suite (see "Mandatory" above) and confirm it is green
   before continuing.

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
  entity, per cell, or per frame (rule 1). Treat **every** shared `Mutex`/`RwLock`
  on a concurrent path as a candidate for a lock-free structure, not just maps —
  see "Prefer lock-free for every shared structure" below.
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
violates, before changing anything. Re-run all tests before moving on.

### Prefer lock-free for every shared structure (not only hashmaps)

The default for any data shared across threads on a hot path is a **lock-free**
representation. Reach for a `Mutex`/`RwLock` only when the access is genuinely
sequential (single-threaded/main-thread-only) or a rare cold path, and say so.
Map each locked structure to its lock-free replacement by *what it does*, not by
its type:

| Locked structure | Lock-free replacement |
|---|---|
| `Mutex/RwLock<HashMap>` with concurrent per-entry insert/remove | `papaya::HashMap` (mutate a value through `retain`'s shared `&V` via an `AtomicU*`/atomic field) |
| `RwLock<Map>` read-mostly + rare **wholesale** replace | `arc-swap::ArcSwap<Arc<Map>>` (lock-free `load`, atomic single-pointer `store` keeps a wholesale swap atomic for readers) |
| `arc-swap` doesn't fit and order matters | `crossbeam-skiplist` (ordered, lock-free) |
| `Mutex<Vec>`/`Mutex<VecDeque>` MPSC/MPMC queue across threads | `crossbeam-queue` (`SegQueue`/`ArrayQueue`), or `crossbeam-channel` |
| `Mutex<Vec>` *collected* from inside a `par_iter_mut` | `bevy::utils::Parallel<T>` (thread-local queues, drain after) |
| `Mutex<counter>` / `Mutex<bool>` / small POD flag | `AtomicU*`/`AtomicBool`/`AtomicUsize` (`Relaxed` unless an ordering edge is needed) |
| `Mutex<single value/config snapshot>` swapped occasionally | `arc-swap::ArcSwap<T>` |
| Per-key cache built once, indexed by a small key | `OnceLock` slot table, or `&'static` baked + leaked (rule 1) |

Rules of thumb: a value mutated through a lock-free container's shared `&V` needs
interior mutability (an atomic field) — never reach for a lock to do the mutation.
A wholesale-replace structure must keep the replace **atomic** for concurrent
readers (single `ArcSwap::store`), or you reintroduce the partial-read race the
lock was preventing — verify which invariant the original lock provided and
preserve it (rule 8). `arc-swap` (v1) and `papaya` (v0.2) are already vendored;
`crossbeam-*` is the next reach if a queue/skiplist is needed.

## Step 2 — Do the work

- **Major issues: fix them now**, without asking. Each fix must be
  behavior-identical (rule 8): same outputs, same error variants, same edge
  cases. Prefer the smallest change that removes the blocker.
- **Minor issues: ask the user** whether to fix each (or batch them) — list them
  with the trade-off. Do not silently apply minor/risky changes.
- After **each** fix: run the full test suite + `cargo check` (per the Mandatory
  section). Add or extend unit tests that pin the preserved behavior. Never bake
  a perf change without proving semantics hold. A red tree halts the pass.

Stay within the architecture conventions in `CLAUDE.md` (one subsystem = one
plugin, gate gameplay on `GameState::InGame`, no narrating comments, etc.).

## Step 3 — Update OPTIMIZATION.md (after each landed optimization)

This is required, not optional. After each major fix (or each batch of related
minor fixes) lands and verifies green:

1. Append an entry to the **"Applied optimizations"** section of
   `OPTIMIZATION.md` — what was slow, what changed, the `file:line` references,
   and which rule(s) it serves. Group tightly-related fixes under one dated
   heading.
2. If the work surfaced a **new general rule** not yet captured, add it to the
   "General rules" section too.
3. If behavior-adjacent docs exist (e.g. `docs/actor.md`), update them per the
   owning subsystem's skill.
4. Re-run all tests after the update (docs-only edits won't break tests, but the
   between-steps rule still applies and keeps the green baseline honest).

Do the update incrementally as you go, not only at the very end, so a partial
pass still leaves an accurate log.

## Finish

Summarize: findings (major fixed / minor pending-or-applied), the test results at
**baseline vs final** (counts), `cargo check` status, and the `OPTIMIZATION.md`
entries you added. Surface any minor issues still awaiting a user decision.

## Guardrails

- Read before code (Step 0). Never optimize hot-path/locking/parallelism code
  without having read `OPTIMIZATION.md`.
- Tests between every step, baseline included. Never advance on a red or
  warning-dirty tree.
- Correctness first: a faster-but-different result is a regression, not an
  optimization. A lock-free swap of a wholesale-replaced structure MUST stay
  atomic for readers — preserve whatever invariant the original lock provided.
- Default to lock-free for shared hot-path structures of **every** kind (maps,
  queues, counters, flags, single values) — see "Prefer lock-free for every
  shared structure." The established lock-free toolkit (`arc-swap`, `papaya`,
  std atomics, `bevy::utils::Parallel`, and `crossbeam-*` when a queue/skiplist is
  needed) is approved; prefer it over a `Mutex`/`RwLock` on any concurrent path.
- Beyond that toolkit, don't add a new top-level dependency to chase performance;
  prefer what Bevy / std already provide.
- Don't micro-optimize cold paths or invent benchmarks the user didn't ask for —
  spend effort where it scales with entities, tiles, or frames.
