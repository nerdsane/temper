# Postgres query-plane checks

A move on the Postgres event store writes its event and every row it changes
(catalog row, field-index rows, key-index rows, vectors, scheduled rows, timer
row, delta mark) in one statement, so these rows cannot fall behind the
journal through a write. The two read-only queries below confirm that on a
live database. Both scan whole tables: run them on a replica or with a
`statement_timeout`, and with `search_path` set to the store's schema.

## Catalog rows behind the journal

Records whose catalog row is older than their last event, or missing. Expect
0 after the first boot on this release: boot repair replays every such record
before the server takes requests.

```sql
WITH h AS (SELECT tenant,entity_type,entity_id,MAX(sequence_nr) s FROM events GROUP BY 1,2,3)
SELECT count(*) FROM h LEFT JOIN entity_catalog c USING (tenant,entity_type,entity_id) WHERE COALESCE(c.sequence_nr,0) < h.s;
```

A deleted record keeps its events, ending in a `Deleted` tombstone, and loses
its catalog row, so it counts here too. To leave deleted records out:

```sql
WITH h AS (
    SELECT DISTINCT ON (tenant, entity_type, entity_id)
        tenant, entity_type, entity_id, sequence_nr AS s, event_type
    FROM events ORDER BY tenant, entity_type, entity_id, sequence_nr DESC
)
SELECT count(*) FROM h LEFT JOIN entity_catalog c USING (tenant, entity_type, entity_id)
WHERE COALESCE(c.sequence_nr, 0) < h.s AND h.event_type <> 'Deleted';
```

## Field-index rows out of step with their catalog row

`entity_field_index` carries no sequence number, so it is checked against the
catalog row it is derived from: recompute each record's scalar index from
`entity_catalog.fields` (strings, numbers and booleans, at most 2000 bytes, as
`scalar_index_fields` in `temper-store-postgres` builds it) and compare. Expect
no rows.

```sql
WITH want AS (
    SELECT c.tenant, c.entity_type, c.entity_id, f.key AS field_name, f.value #>> '{}' AS field_value
    FROM entity_catalog c,
        jsonb_each(CASE WHEN jsonb_typeof(c.fields) = 'object' THEN c.fields ELSE '{}' END) f
    WHERE jsonb_typeof(f.value) IN ('string', 'number', 'boolean')
      AND octet_length(f.value #>> '{}') <= 2000
), have AS (
    SELECT tenant, entity_type, entity_id, field_name, field_value FROM entity_field_index
)
SELECT tenant, entity_type, count(DISTINCT entity_id) AS records_out_of_step
FROM ((TABLE want EXCEPT TABLE have) UNION ALL (TABLE have EXCEPT TABLE want)) d
GROUP BY 1, 2
ORDER BY 3 DESC;
```

A number is compared as Postgres prints it, which can differ from how the
index wrote it for exponent forms such as `1e3`; check such a record by hand
before counting it.

## After each cutover

A rolling deploy runs the old kernel beside this one for a while. The old
kernel writes the catalog and field-index rows in a second transaction after
the event commits, so an old process that stops between the two leaves a
record whose catalog row is behind its journal. Run the catalog-behind check
above on staging and production after each cutover, once the old process is
gone. If it is not 0, restart the service once: boot repair replays those
records before the new process serves. Do not write the rows by hand.
