---- MODULE heartbeat_field_left_alone ----
\* due is the deadline the kernel keeps: Tick lets it fall due, a beat restarts it.
VARIABLES s, f, due
vars == <<s, f, due>>
Init == s = "Idle" /\ f = "x" /\ due = FALSE
Begin == s = "Idle" /\ s' = "A" /\ f' = f /\ due' = FALSE
Beat1 == s = "A" /\ f = "x" /\ f' = "y" /\ s' = s /\ due' = FALSE
Beat2 == s = "A" /\ f = "y" /\ f' = "x" /\ s' = s /\ due' = FALSE
Tick == s = "A" /\ ~due /\ due' = TRUE /\ s' = s /\ f' = f
Expire == s = "A" /\ due /\ s' = "Out" /\ f' = f /\ due' = FALSE
Next == Begin \/ Beat1 \/ Beat2 \/ Tick \/ Expire
Done == s \in {"Idle", "Out"}
Spec == Init /\ [][Next]_vars /\ WF_vars(Begin) /\ WF_vars(Beat1) /\ WF_vars(Beat2) /\ WF_vars(Tick) /\ WF_vars(Expire)
Live == []<>Done
====
