---- MODULE poll_self_left_alone ----
\* One call ends, the next begins. round is the runtime's telling them apart.
\* due is the deadline the kernel keeps: Still puts it back.
VARIABLES s, call, round, due
vars == <<s, call, round, due>>
Init == s = "Idle" /\ call = FALSE /\ round = FALSE /\ due = FALSE
Begin == s = "Idle" /\ s' = "Polling" /\ call' = TRUE /\ round' = round /\ due' = FALSE
Still == s = "Polling" /\ call /\ s' = s /\ call' = TRUE /\ round' = ~round /\ due' = FALSE
Finished == s = "Polling" /\ call /\ s' = "Out" /\ call' = FALSE /\ round' = round /\ due' = FALSE
Failed == s = "Polling" /\ call /\ s' = "Out" /\ call' = FALSE /\ round' = round /\ due' = FALSE
Silent == call /\ s' = s /\ call' = FALSE /\ round' = round /\ due' = due
Tick == s = "Polling" /\ ~due /\ due' = TRUE /\ s' = s /\ call' = call /\ round' = round
Expire == s = "Polling" /\ due /\ s' = "Out" /\ due' = FALSE /\ call' = call /\ round' = round
Next == Begin \/ Still \/ Finished \/ Failed \/ Silent \/ Tick \/ Expire
Done == (s = "Idle" \/ s = "Out") /\ ~call
Spec == Init /\ [][Next]_vars /\ WF_vars(Begin) /\ WF_vars(Still \/ Finished \/ Failed \/ Silent) /\ WF_vars(Tick) /\ WF_vars(Expire)
Live == []<>Done
====
