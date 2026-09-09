# ARN-453 — Intent

Rita's audit after ARN-439 (2026-09-01): "We made CI faster — is anything it does
useless, redundant, or wrong?" Answer: yes, several things. This effort deletes
them and makes what remains accurate. Rulings, verbatim: remove from the pre-push
hook everything CI already does; remove the nightly duplicate; badges must be
accurate, not static; what can be one workflow should be one; don't bench-build
every time.
