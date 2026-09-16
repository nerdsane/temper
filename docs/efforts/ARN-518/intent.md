# ARN-518 — Bounded background continuations

Deliver the existing Temper Foresight application end to end: research produces sourced events and alternative futures; predictions retain their provenance; observed or dated replay outcomes drive visible, evaluated calibration updates.

The real-provider application test exposed a shared runtime blocker. Session ss-01a0aaad-f844-7141-b455-80b84cf8a7d0 received two GPT-5.5 responses and completed one Exa search, then its callback was rejected at 2026-09-16T14:45:40.930485Z with "integration callback depth budget exhausted". Its declared maximum is 40 turns, but the runtime carries an eight-step inline recursion counter through detached background tasks.

The user explicitly approved extending the application work to a scoped Temper kernel continuation fix, preserving loop and authorization protections, on 16 September 2026.

This kernel dependency supports [ARN-518](https://linear.app/arni-build/issue/ARN-518) and [TemperPaw PR #526](https://github.com/nerdsane/temperpaw/pull/526). The existing governed effort is 01a0a7c9-4427-7fd1-8225-603ee2775a55. This is the one kernel PR for that effort; application behavior remains in TemperPaw.

Success means a bounded background workflow crosses more than eight callbacks and finishes, while recursive inline execution and unbounded callback lineages remain bounded. Tenant identity and Cedar authorization must remain unchanged. The full Foresight flow must subsequently pass on the pinned kernel revision before production delivery.
