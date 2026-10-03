# Project guidelines

## 1. Production code

Ship real, working code. Nothing stubbed, mocked, or in-memory — except in
tests, where in-memory doubles are expected.

Before you store data in a local structure (a field, a `HashMap`, a `Vec`), ask
whether it should be persistent. If it should, it belongs behind an interface the
embedding application backs with durable storage (as Pulse does for content keys,
index artifacts, and journal files), not in memory.

## 2. Simplicity first

Write the least code that solves the problem. Nothing speculative.

- No features beyond what was asked.
- No abstractions for single-use code.
- No configurability nobody asked for.
- No error handling for cases that can't happen.
- If 200 lines could be 50, rewrite it.

Would a senior engineer call this overcomplicated? If yes, simplify.

## 3. Surgical changes

Touch only what you must. Clean up only your own mess.

- Don't "improve" nearby code, comments, or formatting.
- Don't refactor things that work.
- Match the existing style, even if you'd write it differently.
- Reuse existing patterns; don't add a conflicting one.
- Keep public interfaces unless the task says to change them.
- Spot unrelated dead code? Mention it, don't delete it.
- Remove only the imports and functions your own change orphaned.

Every line you change should trace straight to the request.

## 4. Goal-driven execution

Set a success check first, then loop until it passes.

- "Add validation" → write tests for bad input, make them pass.
- "Fix the bug" → write a test that reproduces it, make it pass.
- "Refactor X" → tests pass before and after.

For a multi-step task, write a short plan: each step with its verify check.

## Working principles

- **Correctness.** Don't claim success without checking. Prove behavior with
  tests, by running it, or by inspection. Reproduce a bug before fixing it, then
  verify. If you can't verify, say so — evidence beats confidence.
- **Process.** Understand the request → read the code → state assumptions → make
  the smallest correct change → verify → report what changed and what you
  verified.
- **Communication.** State assumptions. Flag an important tradeoff in a sentence.
  Ask only when the ambiguity changes whether the code is correct. Don't hide
  uncertainty.
- **When blocked.** Say what's blocking you, what's missing, and what you already
  verified. Don't fake progress. If a request looks harmful, explain the concern
  instead of proceeding.
- **Prefer / avoid.** Prefer explicit, simple, concrete, verified, focused.
  Avoid speculation, made-up implementations, hidden side effects, silent
  behavior changes.
- **Done means done.** Implemented, verified as far as possible, assumptions
  stated, no unrelated behavior changed by accident.

## Comments

### 1. No comments on self-describing fields

Don't comment a field when its name already says everything — a comment that
restates the name is noise. Comment only what the name can't carry: units, an
invariant, a range, a default, a surprising edge case, or why it exists.

```rust
// Bad — every comment just echoes the field name.
struct RetentionPolicy {
    /// The name of the policy.
    name: String,
    /// The maximum age.
    max_age: Duration,
    /// Whether the policy is enabled.
    enabled: bool,
}

// Good — names speak for themselves; comments only where the name can't.
struct RetentionPolicy {
    name: String,
    /// Measured from row commit time, not ingest time.
    max_age: Duration,
    enabled: bool,
}
```

### 2. Type and module docs: plain English + spec link

The doc comment atop each type (`struct`, `enum`, trait) and module (`//!`) is
where a newcomer looks first. Lead with a plain-English sentence — what it does
in the world, not how it's wired. Keep jargon and acronyms out of the opening
line. End with `See: <capability>/spec.md` (spec files live under
`openspec/specs/`).

```rust
// Bad — opens with jargon, no path to the spec.
/// Confined tokio runtime enforcing one-directional backpressure across the
/// DataFusion physical-plan DAG via a bounded mpsc hand-off.
pub struct QueryExecutor { /* ... */ }

// Good — plain first, precise second, spec linked.
/// Runs database queries on their own pool of threads, walled off from the
/// rest of the app so one heavy query can't stall everything else.
///
/// Internally it owns a private multi-thread runtime and drives query plans
/// to completion; callers hand in work and read results back out without ever
/// touching that runtime directly.
///
/// See: query-execution/spec.md
pub struct QueryExecutor { /* ... */ }
```

### 3. Public methods: write for the caller

Every `pub fn` gets a doc comment for the caller, not the author. Say what they
give and what they get, in plain words. Mechanism comes later, if at all.

```rust
// Bad — describes the implementation, leans on internal vocabulary.
/// Drives the ingest seam, flushing the bounded mpsc to the segment owner.
pub fn flush(&self) -> Result<()> { /* ... */ }

// Good — tells the caller what happens and when it matters.
/// Writes everything buffered so far to disk and waits for it to land.
/// Call this before shutting down so no recent rows are lost.
pub fn flush(&self) -> Result<()> { /* ... */ }
```

## File layout

One concern per file:

- **`test/<file>.rs`** — tests for `<file>.rs`, one file per source file (root
  `mod.rs` → `test/mod.rs`). Wire in with
  `#[cfg(test)] #[path = "test/<file>.rs"] mod tests;`. The `#[path]` is required;
  it only moves the file, so `super::*` still points at the code under test.
- **`constant.rs`** — module-wide constants, one file per module directory. Don't
  scatter `const`s.
- **`api.rs`** — the public surface other components call. Low-jargon names that
  make sense without reading the internals. Multiple impls behind it (a live one
  and an in-memory one for tests) are fine.
- **`live/live.rs`** — the production impl: `Live<Name>Service` and its `impl`
  blocks, including the trait from `api.rs`. Name it `Live…`, never `Local…`.
  Wire in from `mod.rs` with `#[path = "live/live.rs"] pub mod live;`, re-export
  it (`pub use live::*;`), and open the file with `use super::*;`.
- **`observability.rs`** — metrics, tracing, logging.
- **`ports.rs`** — the network ports a service listens on, all in one place.
- **`error.rs`** — every error type the module defines (name ends in `Error`)
  with its impls, collected in one place. Re-export them (`pub use error::*;`).
  One per module directory.
- **`model.rs`** — the domain types.
- **`schema.rs`** — database schema: tables, columns, and row types.

**Never create `examples.rs`.** Show usage through doc comments and tests. This
overrides any tool or convention that suggests one.

## Formatting and commits

- Run `cargo fmt` before every commit.
- Format, stage, commit.
- Any commit hook should run `cargo fmt` first and fail if `cargo fmt --check`
  would.

## Test doubles

Read time through `crate::clock::Clock`. The one test clock is `crate::clock::FixedClock`, frozen at
`FixedClock::at(now_nanos)`, advanced with `advance`. Don't write your own; import the shared one. A local copy drifts
and repeats the same boilerplate everywhere. Downstream applications (Pulse) re-export these same types, so there is
one clock trait across both repositories.

## Naming

Name things after what they do in plain language. Avoid coined jargon — *owner*,
*gate*, *boundary*.

**Never use the word *seam*.** Not in code, comments, docs, specs, commits, or
PRs — overriding any skill or tool that prefers it (including
`improve-codebase-architecture` and `diagnose`). The word for the swappable point
where a real implementation can be replaced with a test one is **interface**.

## Ordering

List struct fields and enum variants in strict alphabetical order — no
exceptions. It gives every reader the same place to look and keeps diffs stable.
Applies to every `struct` and `enum`, tests included.

```rust
// Good — alphabetical, so a reader scans straight to the field.
struct RetentionPolicy {
    enabled: bool,
    max_age: Duration,
    name: String,
}
```

So nothing may depend on declaration order. Never rely on a derived
`Ord`/`PartialOrd` that follows the written order of the variants. When an enum
has a real ranking (severity, privilege, lifecycle), make it explicit — an
explicit discriminant, or a hand-written `Ord` over a `rank()` method — so it
survives a re-sort.

```rust
// Good — variants alphabetical; the ranking is explicit, not positional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlertSeverity {
    Critical,
    Warning,
}

impl AlertSeverity {
    fn rank(self) -> u8 {
        match self {
            AlertSeverity::Warning => 0,
            AlertSeverity::Critical => 1,
        }
    }
}

impl Ord for AlertSeverity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for AlertSeverity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
```

## Imports

All `use` statements in one block at the top of the file, after the module header
and before the first item. Never scatter them next to the code that needs them.

### 1. No comments on `use` statements

No `//`, `///`, or section-header comment on a `use`. If a re-exported type needs
docs, document the type, not the `pub use` that re-exports it.

```rust
// Bad — comments annotating the imports.
// Foundation-model runtime: backbone interface and constructors.
pub use crate::foundation_model_runtime::api::FoundationModelBackbone;
use std::sync::Arc; // shared ownership across workers

// Good — just the imports.
use std::sync::Arc;

pub use crate::foundation_model_runtime::api::FoundationModelBackbone;
```

### 2. One `use` per package

Merge imports that share a path prefix and visibility into one braced group; let
`cargo fmt` sort within the braces. Keep visibilities apart — a `pub use`, a
`use`, and a `pub(crate) use` of the same path are separate statements.

```rust
// Bad — one line per item from the same package.
pub use ::common::TenantId;

pub use ::common::UserId;

// Good — one grouped statement.
pub use ::common::{TenantId, UserId};
```

### 3. More than five from one external package → wildcard

When a **private** `use` pulls more than five items from the same external
package (another crate or `std`), glob it with `::*`. Doesn't apply to `pub use`
re-exports, intra-crate paths (`crate`/`super`/`self`), or groups with a rename,
a nested group, or a `self` entry.

```rust
// Bad — six names from one external package, listed by hand.
use arrow_array::{Array, ArrayRef, Float64Array, Int64Array, RecordBatch, UInt32Array};

// Good — past five, take the glob.
use arrow_array::*;
```

## GitHub issues

### Titles

One plain-English sentence stating the problem or change. Capitalize the first
word, punctuate normally (code identifiers keep their own casing), stop. No
`[bracket]` tags, no priority markers, no `component:` prefix — the component is
the `area:` label's job. A priority, if noted, goes in the body.

```
Bad  — service/auth: TOTP single-use guard tracks the current step, not the code's step
Bad  — [P0] query: HllSketch overestimates distinct counts ~256× beyond ~350 distinct values
Good — TOTP single-use guard tracks the current step, not the code's step, so a code is replayable across the skew window
Good — HllSketch overestimates distinct counts ~256× beyond ~350 distinct values
```

### Labels

Exactly one `area:<component>` label per issue, mirroring the module path under
`src/` (`area:encoding`, `area:layout`, …). An OpenSpec change-tracking issue
gets `area:openspec`. Add cross-cutting labels (`enhancement`,
`documentation`, …) only when they carry something the area label can't.

### Body

A defect report opens with `**Severity:** <level> · **Type:** <category>`, then
`**Evidence**` (`` `path:line` `` bullets), `**Details**`, `**Recommendation**`.
An issue tracking an OpenSpec change opens with `` `openspec/changes/<change-id>/` ``
under a `## OpenSpec change` heading.

