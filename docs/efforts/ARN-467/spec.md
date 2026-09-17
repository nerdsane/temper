# Strict action contracts

An IOA may opt into `automaton.strict_action_params = true`. Its actions accept JSON objects containing only their declared parameters. Optional `action.constraints` compare a required parameter with the persisted pre-action field, require a greater nonnegative integer, require inequality, or require a nonempty string. Invalid input changes neither state nor event history. Numeric comparisons accept JSON integers, matching numeric effect execution.

Fresh strict or constrained actors materialize their declared initial values before accepting input. Creation records typed initial values in its bootstrap event. Full journal replay and snapshots preserve those committed values; replay of an older journal does not invent newly declared defaults. Recovery preserves the stored state. A comparison fails when its target is missing; it never supplies a declaration default to repair that missing value during validation.

Startup writes a bootstrap only when the durable sequence is empty. Skipped legacy events still count as existing history. Lenient recovery may read a legacy bootstrap after skipped events, but committed defaults require the first journal position and authoritative recovery retains its stricter validation.

Native comparisons resolve referenced values through bounded, verified blob reads without changing actor fields. An unresolved reference refuses equality and inequality. Stores that truncate oversized values refuse a write before effects if it would truncate a comparison target.

Strict entities are created with identity and the declared initial status only. Collection creation checks Cedar against the prepared resource before returning verification, status or strict-field errors. Generic field updates and deletion are refused at the actor boundary. Data and lifecycle changes use declared actions. Existing IOAs retain their declared generic-write behavior unless they opt in. A denied or unknown PostgreSQL action leaves fields unchanged, including for non-strict specifications, as native actions already do.

The parser validates constraint names and references. The transition table carries the contract through serialization. Native execution and deterministic simulation execute the same validation before effects. The IOA source in `strict_action_contract.rs` is the executable state model. its simulator assertions express the same safety contract as this document. The existing L0-L3 state verifier does not prove arbitrary external data, provider evidence, or authorization. These extra input preconditions restrict existing transitions. live contract tests are still required.

The reaction simulator projects real actor result fields in the same shape used by production reactions. It must resolve declared cross-entity IDs and parameters from post-action fields.

Cedar authorization evaluates on an eight-megabyte stack, independent of the
request handler's remaining stack. A recursion-limit diagnostic refuses the
request as an engine error even if another permit matched; it cannot silently
discard a forbid or masquerade as a missing permit. The synthetic stack tests
exercise a matching permit, a matching forbid and evaluation-budget exhaustion.

An IOA with strict parameters or parameter constraints declares each action name once. Repeated names cannot replace the contract for an earlier rule. In either case, integer defaults must fit a signed 64-bit integer, and counter defaults must be natural numbers. Comparison targets are declared string, boolean or integer state variables, or the entity identity (`id` or `Id`). The parser rejects floating-point, collection and other field types because their comparison has no runtime contract. Existing list and set actions and guards remain available. Other server-derived fields require dedicated guards because the parameter validator does not receive their values.

Explicit parameter types on strict or constrained actions survive table serialization and validate present values before any effect. Supported types are string/status, bool, signed 64-bit int/integer, and natural-number counter/uint64. Null is not a value of these types; omission remains optional unless a constraint requires the parameter. Bare-name parameters retain their constraint-inferred semantics. Unsupported types and duplicate names in these contracts fail specification validation. Typed parameter declarations must decode as an array of valid names and types; malformed tables never become bare names. Unsigned uint64 parameters can compare against counter fields, but uint64 state fields are not numeric comparison targets because their stored representation is a string. Actor construction compiles transitions and initial state from the same parsed automaton.

A first action whose input violates its parameter contract leaves no actor, index, bootstrap, event, or snapshot. Static and registry specifications use the same lookup for declared child initialization. Existing and durably persisted entities validate their hydrated values at execution time. HTTP and reaction entry points retain their existing Cedar authorization before core dispatch; the public Rust core is not itself a new authorization boundary.


## Turso cleanup contract (September 9)

Replace vendored libSQL with current official Turso packages. Existing event ordering, idempotency, tenant isolation, atomic commit/rollback, authorization persistence, schema upgrades, local files and remote connections remain required. A successful migration removes the vendored source and its checker, changes the storage adapter, and proves the actual runtime path before deployment. No new engine fork or engine patches are permitted. Compatibility qualification may establish a blocker; do not weaken invariants to claim support. ADR-0176 records this decision.

## MCP identity setup contract

ADR-0177 defines the missing setup boundary. `setup_connection` has no tool arguments. Trusted connector configuration determines the service, tenant, operator credential, and private identity file. A native correlated human response authorizes a fixed identity-provisioning policy and a separate nonoperator requester. Decline, cancel, disconnect, malformed content, and unrelated response IDs produce no writes. Ordinary execute calls cannot supply consent, policy text, or credentials.

State model: Unconfigured -> AwaitingHuman -> Consented -> Provisioning -> Verified -> Ready. The first remote write and file creation require Consented; Ready requires a verified requester distinct from the operator. Repeating setup reuses the private credential. Existing inactive or mismatched identities are not overwritten. Responses and credential storage have explicit byte budgets. No secret is returned to the agent. The server's self-resolution rejection is unchanged.

Required proof includes real local Temper provisioning and subsequent denied action -> native test-client response -> operator resolution -> successful retry, plus negative self-approval, zero-write rejection paths, private file checks, reconnection and failure recovery. Production completion separately requires actual human setup and approval responses, identity readback for Genesis and Foresight, local and Foundry verification, recorded review/proof, and one real DSF deployment verified through the twin. Local test-client consent is not production human consent.
