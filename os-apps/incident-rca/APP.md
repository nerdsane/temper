# Incident RCA

Governed incident + root-cause-analysis workflow. A detected failure becomes an **Incident**; the investigation is a state machine, not a chat thread. First deployment: the VM↔Mac mini Tailscale tunnel, whose drops were previously "detected" only by jobs failing and an agent mentioning it in chat.

## The compounding rule

The workflow's reason to exist, enforced as transition guards + invariants in `incident.ioa.toml`:

1. An incident **cannot reach FixProposed** until `RecordPriorReview` names all prior incidents of the same failure mode, and `StateHypothesis` states a root-cause hypothesis **and why previous fixes for this mode did not hold**. "Unknown" is acceptable only when stated explicitly, with what was checked.
2. An incident **cannot close** without recorded verification evidence against explicit criteria (e.g. classifier HEALTHY + dependent jobs green across a window).
3. A recurrence of the same failure mode **reopens** the closed incident and increments `recurrence_count` — the failed closure becomes evidence the next investigation must confront.

## Entity Types

### Incident

**States**: Detected → Investigating → Documented → HypothesisStated → FixProposed → Verifying → Closed, plus Reopened on recurrence.

**Key actions**:
- **BeginInvestigation**: from Detected or Reopened
- **AttachEvidence**: classifier output, job failure reports, log excerpts (required before Documented)
- **RecordPriorReview**: the compounding step — names the prior incidents reviewed
- **CompleteDocumentation**: requires evidence attached
- **StateHypothesis**: requires prior review; records hypothesis + why prior fixes failed
- **ProposeFix**: requires a hypothesis; creates a Remediation
- **BeginVerification / RecordVerification / Close**: closure requires verification evidence
- **Reopen**: recurrence of the same failure mode; increments the counter

**Key fields**: `FailureMode` (e.g. `TUNNEL_DOWN`, `ENROLLMENT_DOWN`, `ADDRESS_APPROVAL_LAPSED`), `SourceSignal` (which job/classifier detected it), `DetectedAt`.

### Remediation

A concrete fix. **States**: Proposed → Approved → Executing → Succeeded / Failed (Retry → Approved), or Rejected.

Adapted from the on-call reference app with one deliberate difference: **no AutoApprove**. In v1 every remediation is human-approved (Cedar: `Approve`/`Reject` are human-only). Auto-approval for proven low-risk fixes is a v2 question, earned by the incident record.

### Postmortem

Agent-drafted analysis of a closed incident. **States**: Draft → InReview → Approved → Published. Root cause required before review; publishing feeds the corpus that `RecordPriorReview` must consult.

## Sensor

`tailnet-diagnose.py` classifies tunnel health (HEALTHY / ENROLLMENT_DOWN / ADDRESS_APPROVAL_LAPSED / TUNNEL_DOWN). On a non-healthy classification from a tunnel-dependent job's failure path, the sensor opens an Incident (or reopens/dedupes by failure mode). Because a down tunnel also blocks VM→Mac API access, detections queue on the side that observed them and file — timestamped at detection — when connectivity allows; a Mac-local probe covers Mac-side state.

## Setup

```
temper.install_app("<tenant>", "incident-rca")
```

Seed corpus: `seeds/tunnel-incidents.json` — the historical tunnel incidents (Oct 2026), loaded as Closed incidents with their recorded evidence; 2026-10-10 is seeded with root cause explicitly unknown (operator fix never recorded).

Plan of record: master-repo-0.1 issue #372.
