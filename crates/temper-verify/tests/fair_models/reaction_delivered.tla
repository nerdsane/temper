---- MODULE reaction_delivered ----
VARIABLES s, t, flight
vars == <<s, t, flight>>

Init == s = "Idle" /\ t = "Waiting" /\ flight = FALSE

Send == s = "Idle" /\ s' = "Sent" /\ flight' = TRUE /\ t' = t

Deliver == flight /\ flight' = FALSE /\ t' = "Taken" /\ s' = s

Next == Send \/ Deliver

Spec == Init /\ [][Next]_vars /\ WF_vars(Deliver)

Done == (s = "Idle" \/ t = "Taken") /\ ~flight

Live == []<>Done
====
