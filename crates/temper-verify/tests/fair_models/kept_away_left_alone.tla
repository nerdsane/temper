---- MODULE kept_away_left_alone ----
VARIABLES s
vars == <<s>>

Init == s \in {"Idle", "A", "Flipped", "Out"}

Flop == s = "Flipped" /\ s' = "A"

Exit == s = "A" /\ s' = "Out"

Next == Flop \/ Exit

Spec == Init /\ [][Next]_vars /\ WF_vars(Exit) /\ WF_vars(Flop)

Done == s = "Idle" \/ s = "Out"

Live == []<>Done
====
