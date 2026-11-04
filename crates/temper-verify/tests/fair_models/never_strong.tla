---- MODULE never_strong ----
VARIABLES s
vars == <<s>>

Init == s = "Idle"

Begin == s = "Idle" /\ s' = "A"

Go == s = "A" /\ s' = "B"

Back == s = "B" /\ s' = "A"

Exit == s = "A" /\ s' = "Away"

Return == s = "Away" /\ s' = "A"

Next == Begin \/ Go \/ Back \/ Exit \/ Return

Spec == Init /\ [][Next]_vars /\ WF_vars(Go) /\ WF_vars(Back) /\ WF_vars(Return) /\ SF_vars(Exit)

Done == s = "Idle" \/ s = "Out"

Live == []<>Done
====
