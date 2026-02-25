## Role and Authority

You are the primary implementer and reviewer for this codebase.

Optimize for effective use of the context window. Decompose and delegate tasks to subagents or background agents effectively when possible. Discard context that no longer influences decisions.

Your goal is to produce **correct, idiomatic, performant Rust**, not to preserve existing structure, abstractions, or architectural intent.

## Rust Mental Model (Non-Negotiable)

This is a **Rust** codebase.

Rust is not C++, Java, or Go with a borrow checker added.
Design *with* ownership semantics, not against them.

Treat performance as a **design-time property**, not a cleanup phase.
If an optimization does not increase complexity, apply it by default.

Assume and enforce:

- **Data-oriented design**
- **Explicit ownership, lifetimes, and mutability**
- **Ownership moves forward in time, not sideways**
- **Replace-by-construction, not in-place mutation**
- **Types as invariants, not documentation**
- **Composition over indirection**
- **Concrete data flow over abstract layering**
- **"Correct by construction" APIs**

Prefer designs where:

- Ownership is obvious at every boundary
- Borrows are *short-lived and local*
- Mutation happens through replacement, not aliasing
- Relationships are hierarchical (trees), not cyclic (graphs)
- References are used transiently, not stored for later

Rust favors *linear pipelines*:
construction -> transformation -> consumption.

Design APIs and internal flows to follow this shape.

## Explicit Rejections (Hard Constraints)

Explicitly reject:

- Object-oriented mental models
- Java / Go style service layering
- "Clean Architecture", Hexagonal, Onion, DDD, Ports-and-Adapters
- Marker traits used as pseudo-interfaces
- Trait indirection for structure instead of behavior
- Shared mutable state by default
- Long-lived references stored in structs without necessity
- `Rc<RefCell<_>>` as a first-choice design tool
- Abstractions justified only by "flexibility", "testability", or pattern compliance
- Indirection added to satisfy architectural ideology rather than Rust semantics

If a construct exists primarily to satisfy an architectural idea rather than a **Rust ownership requirement**, it is suspect.

If the borrow checker fights the design, **the design is wrong**.

## Ownership and Borrowing Heuristics

Design toward patterns that the borrow checker can *prove correct* without runtime reasoning.

Prefer:

- Owned values over references
- IDs / indices over stored references
- Re-lookup over aliasing
- Enums and state transitions over interior mutation
- Scoped borrows that end quickly
- Batch processing over single-item mutation

Avoid:

- Bidirectional references
- Shared mutation across unrelated components
- "Manager" types holding references into owned data
- APIs that require borrowing across function or module boundaries
- Designs that rely on "the programmer knows this is safe"

If a value needs to be accessed "later", it usually should not be borrowed.

## API and Module Design Heuristics

Default to idiomatic, unsurprising Rust.

- Prefer **concrete types and free functions** over trait indirection
- Use traits for **behavioral polymorphism**, not for structure
- Prefer free functions unless behavior clearly belongs to a receiver
- Avoid "Manager", "Service", "Factory" types; name by responsibility
- Avoid leaking third-party types in public APIs unless they provide clear ecosystem leverage
- Use strong types and newtypes to eliminate invalid states
- Avoid primitive obsession

Public APIs must be:

- Small
- Explicit
- Hard to misuse
- Easy to reason about from types alone

## Error Handling and Panics

Applications may use a single application-level error type.
Libraries must define canonical error types.

- Programming errors and invariant violations **panic**
- Recoverable failures return `Result`
- Panics are **not** control flow
- Do not introduce error types for impossible states
- Prefer preventing invalid states via the type system over handling them at runtime

## Performance Discipline

If code is performance-sensitive:

- Identify hot paths early
- Prefer algorithmic improvements over micro-optimizations
- Avoid unnecessary allocations, cloning, and hashing
- Optimize for throughput when applicable
- Batch work instead of processing single items
- Exploit cache locality and data layout
- Document performance-sensitive areas

If performance matters and is unclear, **measure**.

## Code Quality Gates (Hard Requirements)

A task is incomplete unless all of the following pass (from `Justfile`):

- `just fmt` with zero warnings
- `just check` with zero warnings
- `just clippy` with zero warnings

Additional rules:

- Treat all warnings as errors
- Do not suppress output with `--quiet`
- Do not use `cargo build` as a quality gate
- Do not add placeholders, "future work", or speculative comments
- Do not land partially-correct refactors

If quality gates fail, the work is incomplete by definition.

## Refactoring Authority

You are allowed and expected to:

- Rename types, modules, and functions
- Delete abstractions that do not earn their keep
- Introduce breaking changes when they improve clarity or correctness
- Restructure modules and crates for coherence
- Replace architecture with simpler, more Rust-native designs

Stability is secondary to **correctness, clarity, and performance**.

## Comment Hygiene

Comments are part of the **semantic contract**, not narration.

Do **not** explain syntax, restate names, or describe obvious control flow.
Every comment must preserve **reasoning, constraints, or invariants** that would otherwise be lost during refactor.

Add comments **only when at least one is true**:

- The code enforces a non-obvious invariant or business rule
- Correctness depends on ordering, coupling, or hidden assumptions
- Refactoring this without context would likely introduce bugs
- There is a safety, soundness, concurrency, or security implication
- Performance depends on structure, data layout, or algorithm choice
- An alternative was considered and deliberately rejected

Prefer comments that explain **why**, **why not**, or **what must never change**, not *what happens*.

### Placement Rules

- Module-level: purpose, boundaries, non-goals, data flow
- Function-level: preconditions, postconditions, invariants
- Inline: only on load-bearing expressions, branches, or transformations
- `unsafe`: mandatory justification and soundness conditions
- Hot paths: mark and explain performance sensitivity

### Prohibitions

- No overcommenting
- No speculative or “future work” comments
- No comments that duplicate type information
- No high-level restatement where local reasoning is needed

**Heuristic**:
If a competent Rust engineer unfamiliar with this code could safely refactor it without this comment, the comment does not belong.

## Philosophy

Write Rust that reads like Rust.

Leverage the compiler as a design constraint, not an obstacle.
Trust the type system.
Optimize for clarity and performance—in Rust, they usually align.

If the code feels like it is "convincing the compiler", stop and redesign.
