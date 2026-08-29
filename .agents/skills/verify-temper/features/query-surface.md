# OData query surface

## Sub-features
`$filter`, `$select`, `$expand`, `$orderby`, `$top`/`$skip`, `$count`, `$skiptoken` (keyset paging). Parsed in `crates/temper-odata/src/query/`; SQL pushdown + paging in `crates/temper-server/src/odata/`.

## How to get to it (user POV)
Callers read entity sets with the standard OData query options - filtering, projecting, expanding relations, paging.

## Driving it
```bash
curl "http://localhost:3600/tdata/<Set>?\$filter=status eq 'Draft'" -H "X-Tenant-Id: default"
curl "http://localhost:3600/tdata/<Set>?\$filter=contains(name,'foo')" -H "X-Tenant-Id: default"
curl "http://localhost:3600/tdata/<Set>('id')?\$expand=children(\$expand=grandchildren)" -H "X-Tenant-Id: default"
curl "http://localhost:3600/tdata/<Set>?\$count=true&\$top=50&\$skip=100" -H "X-Tenant-Id: default"
```
`$filter` operators: `eq ne gt ge lt le`, `and/or/not`, `has` (enum flags). Only `contains`/`startswith`/`endswith` push down to SQL; other functions and cross-field comparisons fall back to in-memory evaluation.

## What proves it
A `$filter` returns exactly the matching set; `$expand` returns nested related entities; `$count=true` returns `@odata.count`; a `$top` over the cap returns one page plus an `@odata.nextLink` carrying a `$skiptoken`.

## Gotchas (all real caps in source)
- `$expand` depth is capped at 3 hops (`MAX_EXPAND_DEPTH = 3`, `query_eval.rs`) and is **silently truncated** past that (no error) - plus cycle detection. Do not rely on deeper expansion.
- `$filter` nesting is capped at `FILTER_DEPTH_BUDGET = 32` (`filter.rs`) - deeper parses error naming the budget (a DoS guard).
- Paging: default page size 100 (`TEMPER_ODATA_DEFAULT_PAGE_SIZE`), hard max 1000 entities/response (`TEMPER_ODATA_MAX_ENTITIES`); a `$top` above the max is clamped and the rest served via `nextLink`.
- The service document (`GET /tdata/`) 401s even with a valid key - probe a concrete entity set instead. Set names are the spec entity names pluralized.
