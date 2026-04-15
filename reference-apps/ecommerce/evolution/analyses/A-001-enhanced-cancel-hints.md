# A-001: Enhanced Agent.Hint for CancelOrder

**Type:** A-Record (Analysis)
**Timestamp:** 2025-01-15T16:00:00Z
**Origin:** [P-001](../problems/P-001-cancel-agent-guidance.md)

## Proposed Solution

Add a richer `Agent.Hint` annotation to the CancelOrder action in the CSDL
that explicitly tells agents what to do when the order is in a non-cancellable
state.

### CSDL Diff

```xml
<!-- Before -->
<Annotation Term="Temper.Vocab.Agent.Hint"
            String="Cancel an order. Only possible from Draft, Submitted, or Confirmed states."/>

<!-- After -->
<Annotation Term="Temper.Vocab.Agent.Hint"
            String="Cancel an order. Only possible from Draft, Submitted, or Confirmed states.
            Cannot cancel once Processing/Shipped. For shipped orders, use InitiateReturn instead.
            Check order Status before calling — if Processing or later, suggest InitiateReturn to the user."/>
```

### Additional: Add Agent.CommonPattern

```xml
<Annotation Term="Temper.Vocab.Agent.CommonPattern"
            String="1. GET Order to check Status. 2. If Draft/Submitted/Confirmed: POST CancelOrder.
            3. If Shipped/Delivered: suggest InitiateReturn instead. 4. If Processing: inform user
            the order is being prepared and cannot be cancelled."/>
```

## Risk Assessment

- **Risk:** Low (purely additive, metadata only)
- **Blast radius:** Only affects agent behavior, no state machine changes
- **Rollback:** Remove the annotation

## Related

- **D-Record:** [D-001](../decisions/D-001-approve-cancel-hints.md)
