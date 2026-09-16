---------------------- MODULE model ----------------------
EXTENDS Naturals
CONSTANTS InlineLimit, HopLimit
VARIABLES depth, hops
vars == <<depth, hops>>

Init == /\ depth = 0 /\ hops = 0

AdmitCallback ==
    /\ depth < InlineLimit
    /\ hops < HopLimit
    /\ depth' = depth + 1
    /\ hops' = hops + 1

Detach ==
    /\ depth > 0
    /\ depth' = 0
    /\ UNCHANGED hops

Exhausted == /\ hops = HopLimit /\ UNCHANGED vars

Next == AdmitCallback \/ Detach \/ Exhausted
Spec == Init /\ [][Next]_vars

TypeOK == /\ depth \in Nat /\ hops \in Nat
InlineBounded == depth <= InlineLimit
HopsBounded == hops <= HopLimit
HopBudgetNeverResets == [][hops' >= hops]_vars
==========================================================
