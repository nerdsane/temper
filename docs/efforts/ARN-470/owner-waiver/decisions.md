# Decisions

## Separate the owner's disposition from model review

- Decision: Store owner waivers separately from historical review records.
- Came up because: Current-head review selection cannot represent Rita's explicit request to stop reviewing and proceed after verified fixes.
- Options: Relabel earlier review records; disable branch protection; add a distinct exact-head disposition.
- Chose: A distinct disposition because it preserves truthful reviewer provenance and leaves unrelated required checks intact.
- Where: review/owner_waiver.py and gates/sdlc.yml.
