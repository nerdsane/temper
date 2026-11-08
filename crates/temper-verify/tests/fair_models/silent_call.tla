---- MODULE silent_call ----
VARIABLES s, call
vars == <<s, call>>

Init == s = "Idle" /\ call = FALSE

Begin == s = "Idle" /\ s' = "Working" /\ call' = TRUE

Failed == call /\ s = "Working" /\ s' = "Out" /\ call' = FALSE

Silent == call /\ s' = s /\ call' = FALSE

Next == Begin \/ Failed \/ Silent

Spec == Init /\ [][Next]_vars /\ WF_vars(Failed \/ Silent)

Done == s /= "Working" /\ ~call

Live == []<>Done
====
