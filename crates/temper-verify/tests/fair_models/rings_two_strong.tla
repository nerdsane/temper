---- MODULE rings_two_strong ----
VARIABLES s
vars == <<s>>

Init == s = "Idle"

Begin == s = "Idle" /\ s' = "A"

AB == s = "A" /\ s' = "B"

BC == s = "B" /\ s' = "C"

CB == s = "C" /\ s' = "B"

CA == s = "C" /\ s' = "A"

X == s = "A" /\ s' = "Out"

Next == Begin \/ AB \/ BC \/ CB \/ CA \/ X

Spec == Init /\ [][Next]_vars /\ WF_vars(AB) /\ WF_vars(BC) /\ WF_vars(CB) /\ SF_vars(CA) /\ SF_vars(X)

Done == s = "Idle" \/ s = "Out"

Live == []<>Done
====
