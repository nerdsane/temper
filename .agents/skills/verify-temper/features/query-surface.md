# OData query surface

## Sub-features
`$filter`, `$select`, `$expand`, `$orderby`, `$top`/`$skip`, `$count`, `$skiptoken` (keyset paging). Parsed in `crates/temper-odata/src/query/`; SQL pushdown + paging in `crates/temper-server/src/odata/`.

## How to get to it (user POV)
Callers read entity sets with the standard OData query options - filtering, projecting, expanding relations, paging.

## Driving it
Entity-set reads are governed - they need the bearer AND the tenant header (only
`GET /tdata`, `/tdata/`, and `/tdata/$metadata` are public, per
`authz/edge.rs::is_public_kernel_request`). Set `KEY=$TEMPER_API_KEY`:
```bash
A=(-H "Authorization: Bearer $KEY" -H "X-Tenant-Id: default")
curl "http://localhost:3600/tdata/<Set>?\$filter=status eq 'Draft'" "${A[@]}"
curl "http://localhost:3600/tdata/<Set>?\$filter=contains(name,'foo')" "${A[@]}"
curl "http://localhost:3600/tdata/<Set>('id')?\$expand=children(\$expand=grandchildren)" "${A[@]}"
curl "http://localhost:3600/tdata/<Set>?\$count=true&\$top=50&\$skip=100" "${A[@]}"
```
`$filter` operators: `eq ne gt ge lt le`, `and/or/not`, `has` (enum flags). The only supported string functions are `contains`/`startswith`/`endswith` (pushed down to SQL, `filter_sql.rs:395`; also the only ones the in-memory evaluator handles) - any other function or a cross-field comparison is unsupported (`filter_sql.rs:427`, `_ => None`), not silently evaluated.

## What proves it
A `$filter` returns exactly the matching set; `$expand` returns nested related entities; `$count=true` returns `@odata.count`; a `$top` over the cap returns one page plus an `@odata.nextLink` carrying a `$skiptoken`.

## Gotchas (all real caps in source)
- `$expand` depth is capped at 3 hops (`MAX_EXPAND_DEPTH = 3`, `query_eval.rs`) and is **silently truncated** past that (no error) - plus cycle detection. Do not rely on deeper expansion.
- `$filter` nesting is capped at `FILTER_DEPTH_BUDGET = 32` (`filter.rs`) - deeper parses error naming the budget (a DoS guard).
- Paging: default page size 100 (`TEMPER_ODATA_DEFAULT_PAGE_SIZE`), hard max 1000 entities/response (`TEMPER_ODATA_MAX_ENTITIES`); a `$top` above the max is clamped and the rest served via `nextLink`.
- Auth split (measured): `GET /tdata`, `/tdata/` (service document) and `/tdata/$metadata` are PUBLIC (200 without a key); every concrete entity-set read requires the bearer (401 without it, 403 if Cedar denies the principal). Do not copy temperpaw's "service document 401s" gotcha - temper's is public.
- Entity-set names are the CSDL `EntitySet.Name` - read them from `$metadata`, do not hand-pluralize. They ARE the entity types pluralized, but irregularly (measured: `Plan`->`Plans`, `Policy`->`Policies` not `Policys`), so computing the name yourself will miss.
