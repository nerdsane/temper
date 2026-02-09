# Temper Coding Guidelines

Rust coding standards for the Temper framework. Designed for agent-first
development with human break-glass review. Draws from
[TigerStyle](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md),
[Firecracker](https://github.com/firecracker-microvm/firecracker), and
[Apache DataFusion](https://github.com/apache/datafusion).

> **Primary author**: LLM agents.
> **Primary reviewer**: Humans (break-glass).
> Agents write code that passes the verification cascade.
> Humans review specs, security policies, and architectural decisions.

---

## 1. Module Organization

### 1.1 Crate Root (`lib.rs`)

Follow the Firecracker/DataFusion pattern. The crate root is the **public API
surface**. It does three things and nothing else:

1. Crate-level documentation (`//!` doc comments)
2. Module declarations (`pub mod`)
3. Re-exports of primary public types (`pub use`)

```rust
//! temper-jit: Hot-swappable state machine execution.
//!
//! # Usage
//!
//! ```ignore
//! use temper_jit::{TransitionTable, SwapController};
//! ```

pub mod table;
pub mod swap;
pub mod shadow;

// Re-export primary public types at crate root.
// Consumers should never need to spell out internal paths.
pub use table::{TransitionTable, TransitionRule, Guard, Effect};
pub use swap::{SwapController, SwapResult};
pub use shadow::{shadow_test, ShadowResult, TestCase};
```

**Rule**: If a type appears in another crate's function signatures, it MUST be
re-exported from the crate root.

### 1.2 File Size Limits

TigerStyle mandates 70 lines per function because "there's a sharp
discontinuity between fitting on a screen and having to scroll."
The same reasoning applies to files for agent context windows:

| Threshold | Rule |
|-----------|------|
| **≤ 300 lines** | Preferred. Agent reads the whole file in one pass. |
| **301-500 lines** | Acceptable if the file has a single concern. Add a `// SECTION:` comment to mark logical boundaries. |
| **> 500 lines** | **Must split.** Convert flat file to a directory module. |

When splitting, follow TigerStyle "push ifs up, fors down":
- `mod.rs` — re-exports, no logic
- `types.rs` — structs, enums, trait definitions
- Other files — one concern each (parser, builder, evaluator)

### 1.3 Module Layout

Choose ONE pattern per crate and be consistent:

**Flat files** (for crates with ≤ 6 modules, each file ≤ 500 lines):
```
src/
├── lib.rs          # declarations + re-exports
├── table.rs        # TransitionTable, TransitionRule, Guard, Effect
├── swap.rs         # SwapController, SwapResult
└── shadow.rs       # shadow_test, ShadowResult, TestCase
```

**Directory modules** (for crates with complex internal structure):
```
src/
├── lib.rs          # declarations + re-exports
├── actor/
│   ├── mod.rs      # sub-module declarations + re-exports
│   ├── traits.rs   # Actor, Message traits
│   ├── cell.rs     # ActorCell (internal)
│   ├── context.rs  # ActorContext
│   └── actor_ref.rs # ActorRef, Envelope
└── mailbox/
    └── mod.rs      # MailboxSender, MailboxReceiver
```

**Never mix** flat files and directory modules at the same level within a
crate (the `system.rs` next to `actor/` anti-pattern).

### 1.4 File Ordering Within a Module

Follow TigerStyle — read top-to-bottom, most important first:

```rust
// 1. Module-level documentation
//! This module does X.

// 2. Imports (std → external → crate-internal)
use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::actor::ActorRef;

// 3. Constants and type aliases
pub const MAX_ITEMS: usize = 1_000;
pub type EntityId = String;

// 4. Error types
#[derive(Debug, thiserror::Error)]
pub enum MyError { ... }

// 5. Primary public types (the reason this module exists)
pub struct TransitionTable { ... }

// 6. Implementations (impl blocks)
impl TransitionTable { ... }

// 7. Private helper functions
fn extract_guard(...) { ... }

// 8. Tests (always last)
#[cfg(test)]
mod tests { ... }
```

### 1.5 Import Organization

Three groups, separated by blank lines (Firecracker convention):

```rust
// Group 1: Standard library
use std::collections::HashMap;
use std::sync::Arc;

// Group 2: External crates (alphabetical)
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

// Group 3: Crate-internal (alphabetical)
use crate::actor::{ActorRef, Message};
use crate::mailbox::MailboxSender;
```

---

## 2. Assertions (TigerStyle)

### 2.1 Minimum Density

Every function that mutates state MUST have at least two assertions:
one precondition, one postcondition.

```rust
fn apply_transition(&mut self, action: &str) -> Result<(), Error> {
    // PRECONDITION: current state is valid
    debug_assert!(self.valid_states.contains(&self.status));
    debug_assert!(self.events.len() < MAX_EVENTS);

    // ... transition logic ...

    // POSTCONDITION: new state is valid
    debug_assert!(self.valid_states.contains(&self.status));
    debug_assert!(self.events.len() == old_count + 1);
    Ok(())
}
```

### 2.2 Positive and Negative Space

Assert what you expect AND what you don't:

```rust
// Positive: status is in valid set
debug_assert!(self.table.states.contains(&state.status));

// Negative: status is never empty
debug_assert!(!state.status.is_empty());
```

### 2.3 `debug_assert!` vs `assert!`

- `debug_assert!`: Invariants proven by the verification cascade.
  If Stateright checked it exhaustively, use `debug_assert!`.
- `assert!`: Invariants that depend on external input (user data,
  network, config). Always checked, even in release.

### 2.4 No `unwrap()` on External Input

**Forbidden**:
```rust
let value = user_input.parse::<i64>().unwrap(); // NO
```

**Required** (Firecracker rule):
```rust
let value = user_input.parse::<i64>()
    .map_err(|e| ODataError::InvalidValue { ... })?;
```

`unwrap()` is allowed ONLY on operations proven infallible by construction
(e.g., `vec.first().unwrap()` after checking `!vec.is_empty()`), and MUST
be accompanied by a comment explaining why it's safe.

---

## 3. Bounded Execution (TigerStyle)

### 3.1 Every Collection Has a Budget

No unbounded growth. Every Vec, HashMap, queue, and channel has a
compile-time or init-time capacity:

```rust
const MAX_EVENTS_PER_ENTITY: usize = 10_000;
const MAX_ITEMS_PER_ENTITY: usize = 1_000;
const DEFAULT_MAILBOX_CAPACITY: usize = 1_000;
```

### 3.2 Budget Enforcement

Budgets are enforced at the point of insertion, not at the point of query.
Fail fast:

```rust
if state.events.len() >= MAX_EVENTS_PER_ENTITY {
    return Err(Error::BudgetExhausted("events"));
}
state.events.push(event); // Safe: budget just checked
```

### 3.3 No Unbounded Channels

All actor mailboxes use bounded `mpsc::channel(capacity)`.
`mpsc::unbounded_channel()` is forbidden.

---

## 4. Error Handling

### 4.1 Error Type Per Crate

Each crate defines its errors in a dedicated type, using `thiserror`:

```rust
#[derive(Debug, thiserror::Error)]
pub enum MyError {
    #[error("entity not found: {0}")]
    NotFound(String),

    #[error("transition failed: {0}")]
    TransitionFailed(String),

    #[error("persistence error: {0}")]
    Persistence(#[from] PersistenceError),
}
```

### 4.2 Error Propagation

Use `?` for propagation. Map errors at crate boundaries:

```rust
// Good: map the error to our crate's error type
let doc = parse_csdl(xml).map_err(|e| MyError::SpecParse(e.to_string()))?;

// Bad: unwrap
let doc = parse_csdl(xml).unwrap();
```

### 4.3 No `anyhow` in Libraries

`anyhow` is for binaries (CLI, server main). Library crates use typed errors.

---

## 5. Naming (TigerStyle)

### 5.1 Names Capture Understanding

```rust
// Good: clear nouns
struct TransitionTable { ... }
struct EntityState { ... }
fn extract_guard_info(...) -> GuardInfo { ... }

// Bad: vague or abbreviated
struct TT { ... }
struct ES { ... }
fn get_gi(...) -> GI { ... }
```

### 5.2 Units in Names

```rust
const MAX_DELAY_TICKS: u64 = 20;       // Not: MAX_DELAY
const MAILBOX_CAPACITY_MSGS: usize = 1_000;  // Not: CAPACITY
pub duration_ns: u64;                    // Not: duration
```

### 5.3 Consistency

Related names should have the same character count where practical
(TigerStyle alignment principle):

```rust
// Good: aligned
let from_status = state.status.clone();
let to_status = transition.target.clone();

// Acceptable
let source = ...;
let target = ...;

// Bad: asymmetric
let src = ...;
let destination = ...;
```

---

## 6. Functions (TigerStyle)

### 6.1 Size Limit

Target: 70 lines per function (TigerStyle hard limit).
Hard max: 100 lines. Beyond 100, split.

### 6.2 Push `if`s Up, `for`s Down

Parent functions own control flow. Child functions are pure computation:

```rust
// Parent: control flow
fn handle_action(&self, msg: EntityMsg, state: &mut State) {
    match msg {
        EntityMsg::Action { name, params } => {
            if self.budget_exhausted(state) {
                self.reply_budget_error(ctx, state);
                return;
            }
            self.apply_transition(state, &name, &params, ctx);
        }
        EntityMsg::GetState => { ... }
    }
}

// Child: pure computation (no branching on external state)
fn apply_transition(&self, state: &mut State, name: &str, params: &Value) {
    let result = self.table.evaluate(&state.status, state.item_count, name);
    // ...
}
```

---

## 7. Testing

### 7.1 DST-First

Write deterministic simulation tests BEFORE integration tests.
See Section 16 of the Agent Guide.

### 7.2 Test Location

Tests live in the same file as the code they test (Rust convention,
used by Firecracker, DataFusion, and the Rust book):

```rust
// production code
pub fn evaluate(...) -> ... { ... }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluate_valid() { ... }

    #[test]
    fn test_evaluate_invalid() { ... }
}
```

**Exception**: Integration tests that cross module boundaries go in `lib.rs`
or a dedicated `tests/` directory.

### 7.3 Test Fixtures

Test fixtures (sample CSDL, I/O Automaton specs) live in `test-fixtures/` at the
workspace root. Test fixtures must be generic — no domain-specific application names.

### 7.4 Test Naming

```rust
#[test]
fn test_submit_order_requires_items() { ... }     // What it tests
fn dst_cannot_cancel_shipped_order() { ... }       // DST prefix for simulation tests
fn test_parse_example_csdl() { ... }                 // Concrete fixture name
```

---

## 8. Documentation

### 8.1 All Public Items Are Documented

Every `pub fn`, `pub struct`, `pub enum`, `pub trait` has a `///` doc comment.
(Firecracker rule, enforced by `#![warn(missing_docs)]`.)

### 8.2 Crate-Level Docs

Every `lib.rs` has `//!` docs explaining:
1. What this crate does (one sentence)
2. Usage example (`# Usage` section with code)
3. Architecture context (how it fits in the framework)

### 8.3 `# Errors` Section

Functions that return `Result` document their error conditions:

```rust
/// Submit an order for processing.
///
/// # Errors
///
/// Returns `ActionFailed` if:
/// - The order is not in `Draft` status
/// - The order has zero items
/// - The event budget is exhausted
pub fn submit_order(&self, ...) -> Result<..., Error> { ... }
```

---

## 9. Unsafe Code

Heavily discouraged (Firecracker rule). If unavoidable:

```rust
// JUSTIFICATION: tokio requires Send bounds on actor futures,
// and this type is only accessed from a single actor cell.
//
// SAFETY: The ActorCell guarantees single-threaded access to state.
// No concurrent reads or writes are possible by construction.
unsafe { ... }
```

---

## 10. Agent-Specific Rules

### 10.1 Specs Are Source of Truth

Agents modify `.csdl.xml`, `.ioa.toml`, and `.cedar` files.
Agents run `temper codegen` and `temper verify`.
Agents NEVER hand-edit generated code.

### 10.2 Verification Gate

No spec change is deployed without passing the 3-level cascade.
This is not optional. This is TigerStyle's "zero technical debt" —
if the cascade fails, the change doesn't ship.

### 10.3 Human Review Triggers

Agents MUST create evolution records (A-Record) and request human
review for:
- State machine invariant changes (changing what "correct" means)
- Cedar policy changes (security boundary changes)
- Schema migrations (data model changes)
- New entity types (architectural scope changes)

### 10.4 `from_tla_source()` Only

Agents MUST use `TransitionTable::from_tla_source()` in production code.
`from_state_machine()` does not resolve `CanXxx` guard predicates and will
silently produce incorrect guard constraints. This was discovered through
DST and is documented in the paper (Section 11.5).
