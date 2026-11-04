-- Every event of one message is kept (plan/portal/devices.backend.md §23, Q-N5; bench E6-1,
-- 2026-09-28).
--
-- `node_events` was keyed `(hostname, node_key, event_type, ts)`, and every event of a v2
-- `NodeReading` carries its message's `ts`. An alarm page with six events (Node firmware #460,
-- T2c batch) therefore stored only its first alarm: `INSERT OR IGNORE` dropped the other five.
-- "Last alarm" and the restart rule (F24) read this table, and on c58 the restart rule closed
-- slots 1-4 that the Node had raised again.
--
-- `seq` is the event's position in its message, `page_index * 256 + index`, where `index` is
-- its place in the message's `events[]` and `page_index` the `alarm_batch` event's (0 for a
-- message that is not paged). The pages of one batch can arrive within one second, so an index
-- alone would collide again across pages. A redelivered message carries the same events in the
-- same order, so every row keeps its key and is still ignored.
--
-- SQLite cannot change a primary key in place: the table is rebuilt and the rows are copied
-- with `seq = 0`. The table carries no other index, no view and no trigger (V4 created it,
-- V6 renamed `dev_eui` to `node_key`), and nothing references it, so the rebuild is these
-- statements.
--
-- This was V22 on its branch; it lands as V27, above `michal/portal-product-review`'s V17..V23
-- (V25's header has the renumbering).

CREATE TABLE node_events_v27 (
    hostname   TEXT    NOT NULL,
    node_key   TEXT    NOT NULL,
    event_type TEXT    NOT NULL,
    ts         INTEGER NOT NULL,
    seq        INTEGER NOT NULL DEFAULT 0,
    payload    TEXT,
    PRIMARY KEY (hostname, node_key, event_type, ts, seq)
);

INSERT INTO node_events_v27 (hostname, node_key, event_type, ts, seq, payload)
SELECT hostname, node_key, event_type, ts, 0, payload
FROM node_events;

DROP TABLE node_events;

ALTER TABLE node_events_v27 RENAME TO node_events;
