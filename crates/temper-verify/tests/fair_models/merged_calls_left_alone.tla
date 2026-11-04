---- MODULE merged_calls_left_alone ----
VARIABLES s, a, b
vars == <<s, a, b>>
Init == s = "Idle" /\ a = FALSE /\ b = FALSE
Begin == s = "Idle" /\ s' = "P" /\ a' = TRUE /\ b' = TRUE
EndA == a /\ a' = FALSE /\ b' = b /\ s' = IF s \in {"P", "Q"} THEN "Out" ELSE s
AnswerB == b /\ s \in {"P", "Q"} /\ b' = TRUE /\ a' = a /\ s' = IF s = "P" THEN "Q" ELSE "P"
FailB == b /\ b' = FALSE /\ s' = s /\ a' = a
Next == Begin \/ EndA \/ AnswerB \/ FailB
Done == (s = "Idle" \/ s = "Out") /\ ~a /\ ~b
Spec == Init /\ [][Next]_vars /\ WF_vars(Begin) /\ WF_vars(EndA) /\ WF_vars(AnswerB \/ FailB)
Live == []<>Done
====
