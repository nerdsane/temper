# A move that makes a state timer fire now

A `[[state_timeout]]` can list moves in `fire_now_on`. When the record makes one of them while it is in the timer's state, and stays there, its armed timer fires at once instead of at its deadline.

```toml
# A day is the deadline for a delivery GitHub lost, not a poll: GitHub does
# not redeliver a failed delivery by itself, and a person can redeliver one
# for 3 days. A delivery that comes makes the timer fire now.
[[state_timeout]]
state = "Watching"
after_seconds = 86400
on_timeout = "Look"
fire_now_on = ["Heard"]
```

## Why it exists

Timers are deadlines. A record that waits on news from outside, such as a GitHub webhook delivery, keeps a timer as the deadline for news that never came: GitHub records a delivery as failed when the receiver is down or takes longer than 10 seconds to answer, and it does not redeliver a failed delivery by itself. When the news does come, the hook records it with its own move, and that move makes the deadline due now, so the record acts at once. Set the timer's length from how long the outside sender can lose news, not from how often you would like to look.

GitHub's documentation, read 3 Nov 2026:

- "GitHub does not automatically redeliver failed deliveries." ([Handling failed webhook deliveries](https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries))
- "You can redeliver webhook deliveries that occurred in the past 3 days." ([Redelivering webhooks](https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/redelivering-webhooks))
- "Your server should respond with a 2XX response within 10 seconds of receiving a webhook delivery." ([Best practices for using webhooks](https://docs.github.com/en/webhooks/using-webhooks/best-practices-for-using-webhooks))

## Who the fire is

The fire is the timer's own, as if its deadline had come:

- It runs as whoever armed the timer, and that setter's standing is checked as at any fire.
- What it sets off is seen by Cedar as `context.via == "timer"`, with `context.armedBy` naming the setter. A module it calls is checked as a request, as always (temper #203).
- The move that brings it forward lends it no identity and picks no move. A hook that records a delivery cannot use it to act as a person: a person's timer fires as the person, and a timer the hook armed fires as the hook.

Who armed the timer is decided as before. Entering the timed state arms it as the move that entered. A move in `reset_on` arms it again as that move's caller, so a hook move in `reset_on` makes a person's timer the hook's. A move that neither enters nor resets the state, `fire_now_on` moves included, keeps the timer's setter.

## What the checker refuses

The spec does not load when a move in `fire_now_on`:

- is not a declared action, or is an output action;
- cannot be taken in the timer's state without leaving it (leaving the state ends the timer, so it would never find one armed);
- is also in `reset_on` (the reset would make the timer the move's own before it fires);
- is the timer's own `on_timeout`.

## Details

- Only the record's own timer is brought forward. A move on one record never fires another record's timer. The arm keeps its clock, captured arguments and identity, even when its durable row is missing.
- A reaction or module callback in `fire_now_on` is recorded even when it changes no fields: its event brings the timer forward.
- A withheld accelerated fire keeps its saved row and armer. Superseded tasks cannot remove that row when their original deadline arrives.
- A timer that fires once and already fired is not fired again; a repeating timer (its `on_timeout` in `reset_on`) fires once and starts its clock again, as at a deadline. Only deliveries strictly later than that clock start bring the new arm forward; a delivery at the same clock tick as the re-arm cannot fire it again.
- The fire starts right after the move commits. The record's status clock keeps when the move came, so whatever next follows the record's timer (its actor starting again from stored events, its next move, a boot walk that replays it) fires it at once. A process that stops in the moment between the move and the fire, on a record the boot does not replay, fires the timer at its deadline: the deadline still holds.

Regression tests: `cargo test -p temper-server --test timer_fire_now` and `cargo test -p temper-spec fire_now`.
