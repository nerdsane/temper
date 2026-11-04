---- MODULE heartbeat ----
\* due models the deadline the kernel keeps and the joint state does not:
\* Tick lets it fall due, Beat puts it back, Expire can only fire once due.
VARIABLES s, due
vars == <<s, due>>
Init == s = "Idle" /\ due = FALSE
Begin == s = "Idle" /\ s' = "A" /\ due' = FALSE
Beat == s = "A" /\ s' = "A" /\ due' = FALSE
Tick == s = "A" /\ ~due /\ due' = TRUE /\ s' = s
Expire == s = "A" /\ due /\ s' = "Out" /\ due' = FALSE
Next == Begin \/ Beat \/ Tick \/ Expire
Done == s = "Idle" \/ s = "Out"
Spec == Init /\ [][Next]_vars /\ WF_vars(Beat) /\ WF_vars(Tick) /\ WF_vars(Expire)
Live == []<>Done
====
