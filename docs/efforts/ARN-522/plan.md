# Implementation plan

1. `ServerState::has_loaded_actor(tenant, entity_type, entity_id)` — a
   registry lookup, no spawn.
2. In the OData read support, the catalog answers a single-entity read only
   when the actor is not loaded; entity-set materialization drops catalog
   hits for ids whose actor is loaded, so those rows come from the actor.
3. A test that seeds a stale catalog row for an entity whose actor holds a
   newer state and reads it back through the OData path: the actor's state
   wins.
4. Live proof: Genesis's CI round-trip smoke reaches step 7 and the merge
   response reports the merge commit; the re-clone shows it with two parents.
5. One review round; merge; repin genesis #50.
