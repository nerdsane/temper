# Intent

A human approves a Cedar policy and it must be in force. Today `Policy.Activate`
reaches `Active` and the live authorization engine is unchanged, because the
event it emits has no consumer — so every newly installed app's collections
answer 403 until someone makes a manual policy API call (ARN-164). Revoke has
the mirror problem: its hint says "removes it from the authorization engine" and
nothing does.

This effort is the consumer, kept as small as the kernel's own primitives allow.
Split out of [ARN-499](https://linear.app/arni-build/issue/ARN-499) — where a
first version grew into a reconciler that four review rounds kept finding holes
in — on Rita's call to replace it with an elegant solution before proceeding.

Tracked as [ARN-505](https://linear.app/arni-build/issue/ARN-505), under
[ARN-467](https://linear.app/arni-build/issue/ARN-467).
