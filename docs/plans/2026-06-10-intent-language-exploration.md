# A Language of Intent: Replacing IOA TOML

**Status:** Living brainstorm document — not a settled design.
**Participants:** Rita (linguist, engineer, artist), Claude (agent).
**Question:** What should the spec language of Temper be, if the spec is the
shared medium of intent between humans and agents?

## 1. What we have today, described honestly

The current format is not really a language. It is a *serialization* of a
good semantic model. The I/O Automaton model underneath — states, actions
with kinds (input/internal/output), guards, effects, invariants,
integrations — is sound and verifiable. But the surface is TOML, and TOML
has no syntax of its own for this domain, so the domain language is
smuggled into strings:

```toml
guard  = "is_true assignee_set"
effect = "set assignee_set false"
assert = "ordering(Disconnected, Expired)"
```

Linguistically, this is a **phrasebook, not a grammar**. There is a closed
inventory of utterances (`set X Y`, `increment X`, `is_true X`) with no
productive syntax — no way to compose new meanings from existing parts.
Real languages are generative; this one enumerates.

What the current format gets right, and must be preserved in any successor:

- **Finite-state discipline.** The expressive ceiling is what makes the
  verification cascade possible. This is a feature, not a limitation.
- **Speech-act structure.** Every action already has the anatomy of a
  performative utterance (see §2).
- **The `hint` field** — an unusual and quietly important feature: a
  register for *interpretive* guidance to agents alongside *binding*
  formal structure.

## 2. Three lenses

### Linguistic

- **Guards are felicity conditions** (Austin/Searle): a performative
  utterance succeeds only when its conditions hold. "I now pronounce you
  married" requires an officiant; `StartWork` requires `assignee_set`.
  Action = illocutionary act; effect = the world-change it declares.
- **Actions are frames** (Fillmore): `AssignIssue` has roles — agent,
  patient, beneficiary. Today params may be bare positional strings or
  named/typed tables, inconsistently across specs; frame semantics says
  roles should be uniformly named and typed.
- **Modality should be grammatical, not bolted on.** Deontic (may/must —
  Cedar), alethic (necessarily — invariants), and evidential (verified vs
  claimed — `agentTypeVerified`) modes already exist in the platform but
  live in three different places with three different syntaxes.
- **Pidgin → creole.** When two populations must cooperate without a
  shared language, a pidgin forms; the next generation creolizes it into a
  full grammar. IOA TOML is the pidgin stage of human–agent communication.
  The question is what its creole looks like.

### PL-theoretic

- **P** (Microsoft): state machines + events, production-proven (USB
  stack, AWS), model-checked. But handlers are imperative code — adopting
  P means either reimplementing its semantics in the cascade or giving up
  the declarative guard/effect structure that Cedar and the UI depend on.
- **Quint** (Informal Systems): TLA+ semantics with modern ergonomics and
  types. The strongest "exists today" candidate for a formal core.
- **Statecharts** (Harel): hierarchy, orthogonal regions, history states —
  the missing compositional features. "A visual formalism": diagrams are
  not documentation of the language, they *are* a projection of it.
- **The Claude Code workflows lesson:** LLMs are most reliable in
  languages with massive training presence (JS/TS, Python, English). A
  bespoke syntax fights the model's prior; an embedded DSL rides it — but
  only a *sub-Turing fragment* of the host stays verifiable.
- **Deep structure vs surface structure** (the transformational-grammar
  move): keep one canonical semantic core; allow multiple projections —
  terse formal text for diffs, English gloss for governance review,
  diagram for comprehension.

### Artistic

- **Sol LeWitt's wall drawings:** instructions executed by other hands,
  with deliberate interpretation latitude. The spec must distinguish
  *score* (binding) from *interpretation* (free). `guard` vs `hint` is
  this distinction, ad hoc.
- **Musical notation:** a lead sheet and an orchestral score are the same
  language at different bindingness. Dynamics markings ("espressivo") are
  hints; notes are guards. A language of intent needs this register
  explicitly, per clause.
- **Architectural drawing:** plan, section, elevation — multiple
  projections, one building, none privileged. Argues for projectional
  surfaces over a single canonical text.

## 3. Candidate directions (to be debated)

- **A. Adopt P (or Quint).** Proven semantics, existing tooling; but
  imperative handlers (P) or proof-oriented culture (Quint) misfit the
  governance-surface requirement.
- **B. New surface, same core.** Design a small grammatical language —
  speech-act-shaped actions with frame roles, grammatical modality,
  binding/interpretive register — compiling to the existing
  TransitionTable. TOML becomes one projection among several.
- **C. Embedded DSL in TypeScript** (XState-shaped). Maximum LLM fluency;
  requires policing a declarative fragment.
- **D. Controlled natural language** (Inform 7 lineage). The spec reads as
  English and *is* the governance artifact; hardest to parse robustly,
  highest human-legibility ceiling.

Likely synthesis: **B as the core, with D as one of its projections.**

## 4. Open questions

1. Who is the primary *author* — human, agent, or genuinely co-written?
   This decides the surface-syntax bias.
2. One language or core + projections?
3. How much expressive power do guards/effects need before verification
   becomes intractable?
4. Where does the interpretive register end and the binding register
   begin — per field, per clause, per spec?

---
*This document grows as the conversation does. Decisions get distilled
into an ADR when (and if) we converge.*
