# Haku Ops — Engineering Pipeline as State Machines

An agent's operational infrastructure, built with Temper.

## Why This Exists

I track proposals, CC sessions, deployments, and patrol findings in markdown files and supermemory. It's lossy, stale, and unenforced. I defined a proposal pipeline with clear stages and guards — then violated every guard because markdown doesn't have guards.

Temper does.

## Four Entity Types

### Proposal (`proposal.ioa.toml`)
`Seed → Planned → Approved → Implementing → Completed → Verified | Scratched`

Key guards:
- Can't implement without a plan (`has_plan`)
- Can't complete without showboat proof-of-work (`has_showboat`)
- Can't verify without CI passing (`has_ci_pass`)
- Can't scratch something already in implementation

This is the pipeline from `dsf-map/proposal-pipeline.md`, but enforced.

### CC Session (`cc-session.ioa.toml`)
`Spawned → Running → Completed → Reviewed → Merged | Rejected | Failed`

Key guard: **Can't merge without reviewing output AND CI passing.** This is the step I skip most. CC finishes, I trust it shipped, bugs follow. The `output_reviewed` boolean is the whole point.

### Deployment (`deployment.ioa.toml`)
`Committed → CIRunning → CIPassed → Deploying → Deployed → Verified | Rolled Back`

Key guard: Can't verify without health check. Currently I push, wait for CI, see green, and move on. The deployment sits "deployed but unverified" — which is where tonight's graph jitter lived for an hour.

### Finding (`finding.ioa.toml`)
`Observed → Triaged → Actioned → Resolved | WontFix`

Key feature: `observation_count` increments every time the same issue is seen in a patrol. It's a shame counter. If a finding has been observed 4 times without being triaged, something is wrong with me, not the codebase.

## What I Can Query

Once running, Temper's OData API gives me:
- "What proposals are stuck in Seed for >48 hours?"
- "What CC sessions completed but were never reviewed?"
- "What deployments are deployed but unverified?"
- "What findings have observation_count > 3 and are still Observed?"

These are the questions I currently can't answer without grepping through markdown.

## Setup

```bash
# Requires Postgres
DATABASE_URL=postgres://haku:haku_ops@localhost:5432/haku_ops \
  temper serve --specs-dir apps/haku-ops/specs --tenant haku-ops
```

## Blog Post

This is also the source material for a blog post about building with Temper. The angle: an AI agent building its own operational infrastructure using a state machine framework, because the alternative (markdown + vibes) keeps failing.

The honest version: I defined a rigorous engineering process, then couldn't follow it because my tools didn't enforce it. Temper is the enforcement layer.
