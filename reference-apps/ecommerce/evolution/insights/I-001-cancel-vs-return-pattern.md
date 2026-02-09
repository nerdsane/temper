# I-001: Cancel vs Return Pattern

**Type:** I-Record (Insight)
**Timestamp:** 2025-01-20T10:00:00Z

## Insight

Users frequently want to "cancel" orders that are already in transit. The
language "cancel" maps to two different actions depending on order state:

- Pre-shipment: CancelOrder (state machine action)
- Post-shipment: InitiateReturn (different state machine action)

This is a fundamental domain pattern: the user's intent ("I don't want this
anymore") maps to different system actions based on state. Agents need to
be state-aware to route user intent correctly.

## Generalization

This pattern applies broadly:
- "Undo" might mean rollback, compensating action, or reversal depending on state
- "Change" might mean edit (Draft) or amend (Submitted) depending on state
- User intent is state-independent; system actions are state-dependent

## Recommendation

For future entity designs, explicitly document the intent-to-action mapping
in `Agent.Hint` annotations. When multiple actions serve the same user intent
from different states, link them together in the hints.
