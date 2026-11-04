---- MODULE silent_call_timer ----
VARIABLES s, call
vars == <<s, call>>

Init == s = "Idle" /\ call = FALSE

Begin == s = "Idle" /\ s' = "Working" /\ call' = TRUE

Failed == call /\ s = "Working" /\ s' = "Out" /\ call' = FALSE

Silent == call /\ s' = s /\ call' = FALSE

GiveUp == s = "Working" /\ s' = "Out" /\ call' = call

Next == Begin \/ Failed \/ Silent \/ GiveUp

Spec == Init /\ [][Next]_vars /\ WF_vars(Failed \/ Silent) /\ WF_vars(GiveUp)

Done == s /= "Working" /\ ~call

Live == []<>Done
====
