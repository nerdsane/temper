# Serve and the OData surface

## Sub-features
Boot, health, CSDL metadata, entity-set reads, action dispatch.

## Driving it
```bash
cargo run -p temper-cli -- serve --port 3600   # capture PID
curl -sf http://localhost:3600/observe/health
curl -sf http://localhost:3600/tdata/$metadata | head -c 400   # CSDL XML
```
Read an entity set named in the metadata; dispatch an action via `POST /tdata/<Set>('<id>')/Temper.<Action>` with `X-Tenant-Id`.

## What proves it
Health 200 with a live process; metadata is CSDL XML listing the bootstrapped entity types; an entity read returns `@odata.context`. A dispatch is proven by reading the entity back and seeing the state move - a 200 on dispatch alone is not a transition.

## Gotchas
The serve log at bootstrap lists every spec that loaded; a missing entity set means its spec failed the cascade - read the log, not the route.
