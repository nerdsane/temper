---- MODULE always_possible ----
VARIABLES s
vars == <<s>>

Init == s = "Idle"

Begin == s = "Idle" /\ s' = "A"

Go == s = "A" /\ s' = "B"

Back == s = "B" /\ s' = "A"

Exit == s \in {"A", "B"} /\ s' = "Out"

Next == Begin \/ Go \/ Back \/ Exit

Spec == Init /\ [][Next]_vars /\ WF_vars(Go) /\ WF_vars(Back) /\ WF_vars(Exit)

Done == s = "Idle" \/ s = "Out"

Live == []<>Done
====
