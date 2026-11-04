---- MODULE poll_self_left_alone ----
\* One call ends, the next begins. round is the runtime's telling them apart.
VARIABLES s, call, round
vars == <<s, call, round>>
Init == s = "Idle" /\ call = FALSE /\ round = FALSE
Begin == s = "Idle" /\ s' = "Polling" /\ call' = TRUE /\ round' = round
Still == s = "Polling" /\ call /\ s' = s /\ call' = TRUE /\ round' = ~round
Finished == s = "Polling" /\ call /\ s' = "Out" /\ call' = FALSE /\ round' = round
Failed == s = "Polling" /\ call /\ s' = "Out" /\ call' = FALSE /\ round' = round
Next == Begin \/ Still \/ Finished \/ Failed
Done == (s = "Idle" \/ s = "Out") /\ ~call
Spec == Init /\ [][Next]_vars /\ WF_vars(Begin) /\ WF_vars(Still \/ Finished \/ Failed)
Live == []<>Done
====
