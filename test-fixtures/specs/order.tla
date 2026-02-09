---- MODULE Order ----
\* TLA+ Specification for Order Entity State Machine
\* Temper Example: Order State Machine (test fixture)
\*
\* This spec defines the complete lifecycle of an Order entity.
\* The Temper codegen reads this to produce:
\*   1. Rust state machine enum + transition logic
\*   2. Stateright model for exhaustive verification
\*   3. Property test harnesses from invariants
\*
\* Combined with model.csdl.xml for the data model.

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    MAX_ITEMS,        \* Maximum items per order
    MAX_ORDER_TOTAL   \* Maximum order total (business rule)

VARIABLES
    status,           \* Current order status (OrderStatus enum)
    items,            \* Set of items in the order
    total,            \* Computed order total
    payment_status,   \* Associated payment status
    shipment_status,  \* Associated shipment status
    has_address,      \* Whether a shipping address is set
    cancel_reason,    \* Reason for cancellation (if any)
    return_reason     \* Reason for return (if any)

vars == <<status, items, total, payment_status, shipment_status,
          has_address, cancel_reason, return_reason>>

\* ================================================================
\* States
\* ================================================================
OrderStatuses == {"Draft", "Submitted", "Confirmed", "Processing",
                  "Shipped", "Delivered", "Cancelled",
                  "ReturnRequested", "Returned", "Refunded"}

PaymentStatuses == {"None", "Pending", "Authorized", "Captured",
                    "Failed", "Refunded", "PartiallyRefunded"}

ShipmentStatuses == {"None", "Created", "PickedUp", "InTransit",
                     "OutForDelivery", "Delivered", "Failed", "Returned"}

\* ================================================================
\* Initial State
\* ================================================================
Init ==
    /\ status = "Draft"
    /\ items = {}
    /\ total = 0
    /\ payment_status = "None"
    /\ shipment_status = "None"
    /\ has_address = FALSE
    /\ cancel_reason = ""
    /\ return_reason = ""

\* ================================================================
\* Guards (preconditions for transitions)
\* ================================================================
CanAddItem == status = "Draft" /\ Cardinality(items) < MAX_ITEMS

CanRemoveItem(item) == status = "Draft" /\ item \in items

CanSubmit ==
    /\ status = "Draft"
    /\ Cardinality(items) > 0
    /\ has_address = TRUE

CanConfirm ==
    /\ status = "Submitted"
    /\ payment_status = "Authorized"

CanProcess == status = "Confirmed"

CanShip ==
    /\ status = "Processing"
    /\ payment_status = "Captured"

CanDeliver == status = "Shipped"

CanCancel ==
    /\ status \in {"Draft", "Submitted", "Confirmed"}

CanInitiateReturn ==
    /\ status \in {"Shipped", "Delivered"}

CanCompleteReturn == status = "ReturnRequested"

CanRefund ==
    /\ status = "Returned"
    /\ payment_status \in {"Captured", "PartiallyRefunded"}

\* ================================================================
\* Actions (state transitions)
\* ================================================================

\* Add an item to a draft order
AddItem(item) ==
    /\ CanAddItem
    /\ items' = items \union {item}
    /\ total' = total + 1  \* Simplified; real impl computes from prices
    /\ UNCHANGED <<status, payment_status, shipment_status,
                   has_address, cancel_reason, return_reason>>

\* Remove an item from a draft order
RemoveItem(item) ==
    /\ CanRemoveItem(item)
    /\ items' = items \ {item}
    /\ total' = total - 1
    /\ UNCHANGED <<status, payment_status, shipment_status,
                   has_address, cancel_reason, return_reason>>

\* Set shipping address
SetAddress ==
    /\ status = "Draft"
    /\ has_address' = TRUE
    /\ UNCHANGED <<status, items, total, payment_status,
                   shipment_status, cancel_reason, return_reason>>

\* Submit the order (Draft -> Submitted)
SubmitOrder ==
    /\ CanSubmit
    /\ status' = "Submitted"
    /\ payment_status' = "Pending"
    /\ UNCHANGED <<items, total, shipment_status, has_address,
                   cancel_reason, return_reason>>

\* Authorize payment (triggers Submitted -> Confirmed via ConfirmOrder)
AuthorizePayment ==
    /\ status = "Submitted"
    /\ payment_status = "Pending"
    /\ payment_status' = "Authorized"
    /\ UNCHANGED <<status, items, total, shipment_status,
                   has_address, cancel_reason, return_reason>>

\* Payment authorization failed
PaymentFailed ==
    /\ payment_status = "Pending"
    /\ payment_status' = "Failed"
    /\ status' = "Cancelled"
    /\ cancel_reason' = "payment_failed"
    /\ UNCHANGED <<items, total, shipment_status, has_address, return_reason>>

\* Confirm the order (Submitted -> Confirmed)
ConfirmOrder ==
    /\ CanConfirm
    /\ status' = "Confirmed"
    /\ UNCHANGED <<items, total, payment_status, shipment_status,
                   has_address, cancel_reason, return_reason>>

\* Begin processing (Confirmed -> Processing)
ProcessOrder ==
    /\ CanProcess
    /\ status' = "Processing"
    /\ payment_status' = "Captured"
    /\ shipment_status' = "Created"
    /\ UNCHANGED <<items, total, has_address, cancel_reason, return_reason>>

\* Ship the order (Processing -> Shipped)
ShipOrder ==
    /\ CanShip
    /\ status' = "Shipped"
    /\ shipment_status' = "PickedUp"
    /\ UNCHANGED <<items, total, payment_status, has_address,
                   cancel_reason, return_reason>>

\* Deliver the order (Shipped -> Delivered)
DeliverOrder ==
    /\ CanDeliver
    /\ status' = "Delivered"
    /\ shipment_status' = "Delivered"
    /\ UNCHANGED <<items, total, payment_status, has_address,
                   cancel_reason, return_reason>>

\* Cancel the order (Draft/Submitted/Confirmed -> Cancelled)
CancelOrder(reason) ==
    /\ CanCancel
    /\ status' = "Cancelled"
    /\ cancel_reason' = reason
    \* If payment was authorized, it should be voided
    /\ payment_status' = IF payment_status = "Authorized"
                         THEN "Refunded"
                         ELSE payment_status
    /\ UNCHANGED <<items, total, shipment_status, has_address, return_reason>>

\* Initiate return (Shipped/Delivered -> ReturnRequested)
InitiateReturn(reason) ==
    /\ CanInitiateReturn
    /\ status' = "ReturnRequested"
    /\ return_reason' = reason
    /\ UNCHANGED <<items, total, payment_status, shipment_status,
                   has_address, cancel_reason>>

\* Complete return (ReturnRequested -> Returned)
CompleteReturn ==
    /\ CanCompleteReturn
    /\ status' = "Returned"
    /\ shipment_status' = "Returned"
    /\ UNCHANGED <<items, total, payment_status, has_address,
                   cancel_reason, return_reason>>

\* Refund (Returned -> Refunded)
RefundOrder ==
    /\ CanRefund
    /\ status' = "Refunded"
    /\ payment_status' = "Refunded"
    /\ UNCHANGED <<items, total, shipment_status, has_address,
                   cancel_reason, return_reason>>

\* ================================================================
\* Next-state relation (all possible transitions)
\* ================================================================
Next ==
    \/ \E item \in 1..MAX_ITEMS : AddItem(item)
    \/ \E item \in items : RemoveItem(item)
    \/ SetAddress
    \/ SubmitOrder
    \/ AuthorizePayment
    \/ PaymentFailed
    \/ ConfirmOrder
    \/ ProcessOrder
    \/ ShipOrder
    \/ DeliverOrder
    \/ \E reason \in {"customer_request", "out_of_stock", "fraud"} : CancelOrder(reason)
    \/ \E reason \in {"defective", "wrong_item", "changed_mind"} : InitiateReturn(reason)
    \/ CompleteReturn
    \/ RefundOrder

\* ================================================================
\* Safety Invariants (must ALWAYS hold)
\* ================================================================

\* Status is always a valid state
TypeInvariant ==
    /\ status \in OrderStatuses
    /\ payment_status \in PaymentStatuses
    /\ shipment_status \in ShipmentStatuses
    /\ total >= 0

\* Cannot ship without payment captured
ShipRequiresPayment ==
    status = "Shipped" => payment_status = "Captured"

\* Cannot submit without items
SubmitRequiresItems ==
    status = "Submitted" => Cardinality(items) > 0

\* Cannot submit without address
SubmitRequiresAddress ==
    status \in {"Submitted", "Confirmed", "Processing", "Shipped", "Delivered"}
        => has_address = TRUE

\* Cancelled orders stay cancelled (no resurrection)
CancelledIsFinal ==
    (status = "Cancelled") => (status' = "Cancelled" \/ UNCHANGED status)

\* Delivered orders can only go to ReturnRequested
DeliveredTransitions ==
    (status = "Delivered") =>
        (status' \in {"Delivered", "ReturnRequested"})

\* Refunded is a terminal state
RefundedIsFinal ==
    (status = "Refunded") => (status' = "Refunded" \/ UNCHANGED status)

\* Payment consistency: refunded implies order is cancelled, returned, or refunded
PaymentRefundConsistency ==
    (payment_status = "Refunded") =>
        (status \in {"Cancelled", "Returned", "Refunded"})

\* All safety invariants combined
SafetyInvariant ==
    /\ TypeInvariant
    /\ ShipRequiresPayment
    /\ SubmitRequiresItems
    /\ SubmitRequiresAddress
    /\ PaymentRefundConsistency

\* ================================================================
\* Liveness Properties (something good eventually happens)
\* ================================================================

\* A submitted order is eventually confirmed, cancelled, or payment fails
\* (no order stuck in Submitted forever)
SubmittedProgress ==
    (status = "Submitted") ~>
        (status \in {"Confirmed", "Cancelled"})

\* A processing order eventually ships or is cancelled
ProcessingProgress ==
    (status = "Processing") ~>
        (status \in {"Shipped", "Cancelled"})

\* A return request is eventually completed
ReturnProgress ==
    (status = "ReturnRequested") ~>
        (status = "Returned")

\* ================================================================
\* Specification
\* ================================================================
Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

\* ================================================================
\* Model checking configuration
\* ================================================================
ASSUME MAX_ITEMS \in Nat /\ MAX_ITEMS > 0
ASSUME MAX_ORDER_TOTAL \in Nat /\ MAX_ORDER_TOTAL > 0

====
