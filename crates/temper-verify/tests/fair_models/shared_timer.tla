---- MODULE shared_timer ----
VARIABLES s
vars == <<s>>
Init == s = "Idle"
Begin == s = "Idle" /\ s' = "A"
X == s = "A" /\ s' = "B"
Y == s = "B" /\ s' = "A"
GoA == s = "A" /\ s' = "Out"
GoB == s = "B" /\ s' = "Out"
Next == Begin \/ X \/ Y \/ GoA \/ GoB
Done == s = "Idle" \/ s = "Out"
Spec == Init /\ [][Next]_vars /\ WF_vars(X) /\ WF_vars(Y) /\ WF_vars(GoA) /\ WF_vars(GoB)
Live == []<>Done
====
