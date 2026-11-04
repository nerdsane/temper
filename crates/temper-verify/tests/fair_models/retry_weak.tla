---- MODULE retry_weak ----
VARIABLES s, call
vars == <<s, call>>

Init == s = "Idle" /\ call = FALSE

Begin == s = "Idle" /\ s' = "Calling" /\ call' = TRUE

Answer == call /\ s = "Calling" /\ s' = "Out" /\ call' = FALSE

Failed == call /\ s = "Calling" /\ s' = "Waiting" /\ call' = FALSE

Silent == call /\ s' = s /\ call' = FALSE

TimesOut == s = "Calling" /\ s' = "Waiting" /\ call' = call

Retry == s = "Waiting" /\ s' = "Calling" /\ call' = TRUE

Next == Begin \/ Answer \/ Failed \/ Silent \/ TimesOut \/ Retry

Spec == Init /\ [][Next]_vars /\ WF_vars(Answer \/ Failed \/ Silent) /\ WF_vars(TimesOut) /\ WF_vars(Retry)

Done == (s = "Idle" \/ s = "Out") /\ ~call

Live == []<>Done
====
