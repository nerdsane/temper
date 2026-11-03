//! The one statement that writes the journal and its rows.

use crate::schema::tombstone_sql;

/// The append statement. Parameters, in order:
/// `$1..$3` the record; `$4` the expected sequence; `$5..$7` the events' types,
/// payloads and metadata; `$8, $9` the declared keys; `$10` whether the record's
/// vector rows become `$11..$13`; `$14..$17` the scheduled actions armed; `$18`
/// the one fired; `$19` the projection (`upsert`, `remove` or NULL) with
/// `$20..$26` its status, fields, stored state, hash, index names and values
/// and whether its fields are wrapped where the index cannot see; `$27` whether
/// the events hold the tombstone; `$28` the timer (`arm`, `end` or NULL) with
/// `$29..$34` its row.
pub(crate) const APPEND_COMMIT_SQL: &str = concat!(
    "WITH locked AS (
    SELECT c.sequence_nr, c.projection_hash FROM {schema}entity_catalog c
    WHERE c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = $3
    FOR UPDATE),
head AS (
    SELECT COALESCE(MAX(sequence_nr), 0)::bigint AS seq FROM {schema}events
    WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3),
clash AS (
    SELECT k.key_name, k.entity_id FROM {schema}entity_key_index k
    JOIN unnest($8::text[], $9::text[]) AS nk(key_name, key_hash)
      ON k.key_name = nk.key_name AND k.key_hash = nk.key_hash
    WHERE k.tenant = $1 AND k.entity_type = $2 AND k.entity_id <> $3
    ORDER BY k.key_name LIMIT 1),
prev AS (
    SELECT c.sequence_nr, c.projection_hash FROM {schema}entity_catalog c
    WHERE c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = $3),
raced AS (
    SELECT NOT ((NOT EXISTS (SELECT 1 FROM locked) AND NOT EXISTS (SELECT 1 FROM prev))
        OR EXISTS (SELECT 1 FROM locked l JOIN prev p
            ON l.sequence_nr = p.sequence_nr AND l.projection_hash = p.projection_hash))
        AS raced),
chk AS (
    SELECT head.seq AS head, raced.raced,
        head.seq = $4::bigint AND NOT EXISTS (SELECT 1 FROM clash) AND NOT raced.raced AS ok
    FROM head, raced),
open_seg AS (
    SELECT segment_index FROM {schema}event_segments
    WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND sealed_at IS NULL
    ORDER BY segment_index DESC LIMIT 1),
seg AS (
    SELECT COALESCE((SELECT segment_index FROM open_seg),
        (SELECT COALESCE(MAX(segment_index), 0) FROM {schema}events
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3)) AS idx,
        EXISTS (SELECT 1 FROM open_seg) AS is_open),
ev AS (
    INSERT INTO {schema}events
        (tenant, entity_type, entity_id, sequence_nr, segment_index, event_type, payload, metadata)
    SELECT $1, $2, $3, $4::bigint + e.ord, seg.idx, e.event_type, e.payload, e.metadata
    FROM unnest($5::text[], $6::jsonb[], $7::jsonb[])
        WITH ORDINALITY AS e(event_type, payload, metadata, ord), seg, chk
    WHERE chk.ok
    RETURNING sequence_nr),
w AS (
    SELECT COALESCE((SELECT MAX(sequence_nr) FROM ev), $4::bigint) AS seq
    FROM chk WHERE chk.ok),
seg_new AS (
    INSERT INTO {schema}event_segments AS s
        (tenant, entity_type, entity_id, segment_index, start_sequence_nr, end_sequence_nr, event_count)
    SELECT $1, $2, $3, seg.idx, $4::bigint + 1, w.seq, w.seq - $4::bigint
    FROM seg, w WHERE NOT seg.is_open AND w.seq > $4::bigint
    ON CONFLICT (tenant, entity_type, entity_id, segment_index) DO UPDATE SET
        end_sequence_nr = EXCLUDED.end_sequence_nr,
        event_count = GREATEST(EXCLUDED.end_sequence_nr - s.start_sequence_nr + 1, 0)),
seg_upd AS (
    UPDATE {schema}event_segments s SET end_sequence_nr = w.seq,
        event_count = GREATEST(w.seq - s.start_sequence_nr + 1, 0)
    FROM seg, w
    WHERE seg.is_open AND w.seq > $4::bigint
      AND s.tenant = $1 AND s.entity_type = $2 AND s.entity_id = $3
      AND s.segment_index = seg.idx),
held AS (
    SELECT k.key_name, k.key_hash FROM {schema}entity_key_index k
    JOIN unnest($8::text[], $9::text[]) AS nk(key_name, key_hash)
      ON k.key_name = nk.key_name AND k.key_hash = nk.key_hash
    WHERE k.tenant = $1 AND k.entity_type = $2 AND k.entity_id = $3),
key_keep AS (
    UPDATE {schema}entity_key_index k SET sequence_nr = w.seq FROM held h, w
    WHERE k.tenant = $1 AND k.entity_type = $2
      AND k.key_name = h.key_name AND k.key_hash = h.key_hash),
key_drop AS (
    DELETE FROM {schema}entity_key_index k
    USING unnest($8::text[], $9::text[]) AS nk(key_name, key_hash), w
    WHERE k.tenant = $1 AND k.entity_type = $2 AND k.entity_id = $3
      AND k.key_name = nk.key_name AND k.key_hash <> nk.key_hash),
key_add AS (
    INSERT INTO {schema}entity_key_index
        (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr)
    SELECT $1, $2, nk.key_name, nk.key_hash, $3, w.seq
    FROM unnest($8::text[], $9::text[]) AS nk(key_name, key_hash), w
    WHERE NOT EXISTS (SELECT 1 FROM held h
        WHERE h.key_name = nk.key_name AND h.key_hash = nk.key_hash)),
vec_drop AS (
    DELETE FROM {schema}entity_vector_index v USING w
    WHERE $10::boolean AND v.tenant = $1 AND v.entity_type = $2 AND v.entity_id = $3
      AND NOT EXISTS (SELECT 1 FROM unnest($11::text[], $12::text[]) AS nv(decl_name, model_tag)
          WHERE nv.decl_name = v.decl_name AND nv.model_tag = v.model_tag)),
vec_put AS (
    INSERT INTO {schema}entity_vector_index AS v
        (tenant, entity_type, decl_name, model_tag, entity_id, vector, sequence_nr)
    SELECT $1, $2, nv.decl_name, nv.model_tag, $3, nv.vector, w.seq
    FROM unnest($11::text[], $12::text[], $13::bytea[]) AS nv(decl_name, model_tag, vector), w
    WHERE $10::boolean
    ON CONFLICT (tenant, entity_type, decl_name, model_tag, entity_id) DO UPDATE SET
        vector = EXCLUDED.vector, sequence_nr = EXCLUDED.sequence_nr),
sched_add AS (
    INSERT INTO {schema}scheduled_actions
        (tenant, entity_type, entity_id, arm_id, action, due_at, armed_by)
    SELECT $1, $2, $3, s.arm_id, s.action, s.due_at, s.armed_by
    FROM unnest($14::text[], $15::text[], $16::text[], $17::text[])
        AS s(arm_id, action, due_at, armed_by), w
    WHERE s.arm_id IS DISTINCT FROM $18::text
    ON CONFLICT (tenant, entity_type, entity_id, arm_id) DO NOTHING),
sched_fire AS (
    DELETE FROM {schema}scheduled_actions s USING w
    WHERE s.tenant = $1 AND s.entity_type = $2 AND s.entity_id = $3
      AND s.arm_id = $18::text),
go AS (
    SELECT w.seq,
        NOT EXISTS (SELECT 1 FROM prev WHERE prev.projection_hash = $23::text) AS reindex
    FROM w
    WHERE $19::text = 'upsert'
      AND NOT EXISTS (SELECT 1 FROM prev WHERE prev.sequence_nr > w.seq)),
cat_put AS (
    INSERT INTO {schema}entity_catalog AS c
        (tenant, entity_type, entity_id, status, fields, state, sequence_nr,
         projection_version, projection_hash, updated_at, first_event_at)
    SELECT $1, $2, $3, $20::text, $21::jsonb, $22::jsonb, go.seq, 2, $23::text, now(),
        $22::jsonb->>'first_event_at'
    FROM go
    ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET
        status = EXCLUDED.status,
        fields = CASE WHEN c.projection_hash IS DISTINCT FROM EXCLUDED.projection_hash
            THEN EXCLUDED.fields ELSE c.fields END,
        sequence_nr = EXCLUDED.sequence_nr,
        projection_version = 2,
        projection_hash = EXCLUDED.projection_hash,
        first_event_at = COALESCE(c.first_event_at, EXCLUDED.first_event_at),
        state = EXCLUDED.state,
        updated_at = now()
    WHERE c.sequence_nr <= EXCLUDED.sequence_nr
    RETURNING 1),
cat_drop AS (
    DELETE FROM {schema}entity_catalog c USING w
    WHERE $19::text = 'remove'
      AND c.tenant = $1 AND c.entity_type = $2 AND c.entity_id = $3
    RETURNING 1),
cat AS (
    SELECT (SELECT count(*) FROM cat_put) + (SELECT count(*) FROM cat_drop) AS written),
fi_drop AS (
    DELETE FROM {schema}entity_field_index f USING w, cat
    WHERE f.tenant = $1 AND f.entity_type = $2 AND f.entity_id = $3
      AND ($19::text = 'remove' OR EXISTS (SELECT 1 FROM go
          WHERE go.reindex AND f.field_name <> ALL($24::text[])))),
fi_put AS (
    INSERT INTO {schema}entity_field_index AS f
        (tenant, entity_type, entity_id, field_name, field_value)
    SELECT $1, $2, $3, i.field_name, i.field_value
    FROM unnest($24::text[], $25::text[]) AS i(field_name, field_value), go, cat
    WHERE go.reindex
    ON CONFLICT (tenant, entity_type, entity_id, field_name) DO UPDATE SET
        field_value = EXCLUDED.field_value, status = NULL
    WHERE f.field_value IS DISTINCT FROM EXCLUDED.field_value),
mark AS (
    INSERT INTO {schema}entity_projection_delta AS m
        (tenant, entity_type, entity_id, sequence_nr, terminal)
    SELECT $1, $2, $3,
        CASE WHEN $19::text = 'remove' THEN GREATEST(w.seq, COALESCE(
            (SELECT sn.sequence_nr FROM {schema}snapshots sn
             WHERE sn.tenant = $1 AND sn.entity_type = $2 AND sn.entity_id = $3), 0))
        ELSE w.seq END,
        $27::boolean OR ($19::text = 'remove' AND EXISTS (SELECT 1 FROM {schema}events d
            WHERE d.tenant = $1 AND d.entity_type = $2 AND d.entity_id = $3 AND ",
    tombstone_sql!(),
    "))
    FROM w, cat
    WHERE CASE $19::text
        WHEN 'remove' THEN true
        WHEN 'upsert' THEN $26::boolean OR NOT EXISTS (SELECT 1 FROM go)
        ELSE w.seq > $4::bigint END
    ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET
        sequence_nr = GREATEST(m.sequence_nr, EXCLUDED.sequence_nr),
        terminal = m.terminal OR EXCLUDED.terminal),
retire AS (
    DELETE FROM {schema}entity_projection_delta m USING go, cat
    WHERE NOT $26::boolean
      AND m.tenant = $1 AND m.entity_type = $2 AND m.entity_id = $3
      AND m.sequence_nr <= go.seq AND NOT m.terminal
      AND CASE WHEN go.reindex THEN cardinality($24::text[]) > 0
          ELSE EXISTS (SELECT 1 FROM {schema}entity_field_index f
              WHERE f.tenant = $1 AND f.entity_type = $2 AND f.entity_id = $3) END),
timer_arm AS (
    INSERT INTO {schema}state_timers AS t
        (tenant, entity_type, entity_id, state, arm_id, sequence_nr, started_at, params, armed_by, due_at)
    SELECT $1, $2, $3, $29::text, $30::text, $31::bigint, $32::text, $33::text, $34::text, $35::text
    FROM w WHERE $28::text = 'arm'
    ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET
        state = EXCLUDED.state,
        arm_id = EXCLUDED.arm_id,
        sequence_nr = EXCLUDED.sequence_nr,
        started_at = EXCLUDED.started_at,
        due_at = EXCLUDED.due_at,
        params = EXCLUDED.params,
        armed_by = EXCLUDED.armed_by
    WHERE EXCLUDED.sequence_nr >= t.sequence_nr),
timer_end AS (
    DELETE FROM {schema}state_timers t USING w
    WHERE $28::text = 'end'
      AND t.tenant = $1 AND t.entity_type = $2 AND t.entity_id = $3)
SELECT chk.ok, chk.head, (SELECT seq FROM w),
    (SELECT key_name FROM clash), (SELECT entity_id FROM clash), chk.raced
FROM chk"
);
