# Strict action contracts

An IOA may opt into `automaton.strict_action_params = true`. Its actions accept JSON objects containing only their declared parameters. Optional `action.constraints` compare a required parameter with the persisted pre-action field, require a greater nonnegative integer, require inequality, or require a nonempty string. Invalid input changes neither state nor event history. Numeric comparisons accept JSON integers, matching numeric effect execution.

Strict entities are created with identity and the declared initial status only. Generic field updates and deletion are refused at the actor boundary. data and lifecycle changes use declared actions. Existing IOAs retain their declared generic-write behavior unless they opt in.

The parser validates constraint names and references. The transition table carries the contract through serialization. Native execution and deterministic simulation execute the same validation before effects. The IOA source in `strict_action_contract.rs` is the executable state model. its simulator assertions express the same safety contract as this document. The existing L0-L3 state verifier does not prove arbitrary external data, provider evidence, or authorization. These extra input preconditions restrict existing transitions. live contract tests are still required.

The reaction simulator projects real actor result fields in the same shape used by production reactions. It must resolve declared cross-entity IDs and parameters from post-action fields.

An IOA with strict parameters or parameter constraints declares each action name once. Repeated names cannot replace the contract for an earlier rule. Strict integer defaults must fit a signed 64-bit integer, and counter defaults must be natural numbers. Comparison targets are declared state variables or the entity identity (`id` or `Id`). Other server-derived fields require dedicated guards because the parameter validator does not receive their values.
