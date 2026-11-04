---- MODULE merged_calls_left_alone ----
\* Every status counts, so what the rule asks is that no call stays open. b goes
\* round and leaves a gap at Q, which is the rule met once a has ended.
VARIABLES s, a, b
vars == <<s, a, b>>
Init == s = "Idle" /\ a = FALSE /\ b = FALSE
Begin == s = "Idle" /\ s' = "P" /\ a' = TRUE /\ b' = TRUE
EndA == a /\ a' = FALSE /\ b' = b /\ s' = IF s \in {"P", "Q"} THEN "Out" ELSE s
SilentA == a /\ a' = FALSE /\ b' = b /\ s' = s
AnswerB == b /\ s = "P" /\ s' = "Q" /\ b' = FALSE /\ a' = a
SilentB == b /\ b' = FALSE /\ s' = s /\ a' = a
Back == s = "Q" /\ s' = "P" /\ b' = TRUE /\ a' = a
Next == Begin \/ EndA \/ SilentA \/ AnswerB \/ SilentB \/ Back
Done == ~a /\ ~b
Spec == Init /\ [][Next]_vars /\ WF_vars(Begin) /\ WF_vars(EndA \/ SilentA) /\ WF_vars(AnswerB \/ SilentB) /\ WF_vars(Back)
Live == []<>Done
====
