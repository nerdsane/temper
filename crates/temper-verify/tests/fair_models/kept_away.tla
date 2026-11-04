---- MODULE kept_away ----
VARIABLES s
vars == <<s>>

Init == s = "Idle"

Begin == s = "Idle" /\ s' = "A"

Flip == s = "A" /\ s' = "Flipped"

Flop == s = "Flipped" /\ s' = "A"

Exit == s = "A" /\ s' = "Out"

Next == Begin \/ Flip \/ Flop \/ Exit

Spec == Init /\ [][Next]_vars /\ WF_vars(Exit) /\ WF_vars(Flop)

Done == s = "Idle" \/ s = "Out"

Live == []<>Done
====
