---- MODULE nobody_comes ----
VARIABLES s
vars == <<s>>

Init == s = "Idle"

Begin == s = "Idle" /\ s' = "A"

Exit == s = "A" /\ s' = "Out"

Next == Begin \/ Exit

Spec == Init /\ [][Next]_vars

Done == s = "Idle" \/ s = "Out"

Live == []<>Done
====
