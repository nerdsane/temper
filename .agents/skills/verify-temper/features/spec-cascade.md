# Spec verification cascade (L0-L3)

## Sub-features
IOA parse, TransitionTable build, model checking, DST invariants.

## Driving it
```bash
cargo run -p temper-cli -- verify <path/to/spec.ioa.toml>      # single spec
scripts/verify-cascade.sh                                       # all spec dirs, results in .cascade-results/
```

## What proves it
The cascade reports each level passed for the changed spec. An edit that adds a state or action must show the new element in the pass output. A deliberately broken guard must FAIL the cascade - if it passes, that is a finding in the verifier, not a success.

## Gotchas
The `.claude` hook runs this automatically on `.ioa.toml` edits and BLOCKS on failure; running it yourself first avoids losing the edit loop. `.cascade-results/` is local state, never committed.
