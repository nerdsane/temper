# A loaded actor is the source of truth

An entity's actor, when it is in memory, holds the state every dispatch has
applied to it. The entity catalog is a projection of that state, written
behind the dispatch by a queue so that writes do not wait on it. The catalog
exists to answer reads for entities that are not in memory without waking
them.

So the rule for a read is: if the entity's actor is loaded, ask the actor;
otherwise the catalog may answer. A read that follows a dispatch on the same
entity always finds the actor loaded — the dispatch loaded it — and therefore
sees the dispatch's result. A cold entity is served from the catalog as
before, and the shadow check that already repairs catalog drift is
unchanged.

This holds for single-entity reads and for the per-id catalog lookups that
entity-set materialization performs.
