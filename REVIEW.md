# Reviewing temper

Repo-specific passes on top of the global review bar. Severity + `file:line` + concrete failure scenario for every finding.

## Pass 1: Determinism (DST)

Any touched code in `temper-runtime`, `temper-jit`, `temper-server`: scan against the DST ruleset in `.agents/agents/dst-reviewer.md`. Wall clock, random UUIDs, HashMap iteration, thread spawns, direct I/O, and global state are all findings even when tests pass - they break seeded reproduction. Check that new `// determinism-ok` suppressions are justified, not convenient.

## Pass 2: Invariants and the spec contract

- A spec change and its TransitionTable behavior must say the same thing; look for code paths that bypass the table.
- New states or actions without `[[invariant]]` coverage are a finding.
- Framework code hardcoding entity-specific state names is a finding.
- `from_tla_source()` outside `#[cfg(test)]` is a finding.

## Pass 3: Authorization fail-closed

Every new route, action dispatch, and effect path goes through Cedar with an explicit principal. Look for: `is_system` -style bypasses, handlers that default-allow on policy load failure, tenant id taken from request data instead of authenticated context, `agentTypeVerified: false` principals granted trust.

## Pass 4: TigerStyle bounds

Unbounded mailboxes, queues, or retries; missing pre/post assertions on new pub functions; limits where budgets belong; error paths that swallow instead of log-or-propagate; `unwrap`/`expect` on external input.

## Pass 5: Dependency discipline

`temper-jit` gaining a `temper-verify` dependency, or production binaries pulling `stateright`/`proptest`, fails the review outright.
