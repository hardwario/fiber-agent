-- One metadata component for every device kind (plan/portal/metadata.cs.md):
-- a wired probe gets a description of its own (scope 'probe', entity = the
-- wire line 0..7) and every row a `location`. The location is portal-side
-- only for a Hub, which has none on the device; Nodes, Beacons and probes
-- keep theirs in the Hub's state and leave the column NULL.
--
-- SQLite cannot widen a CHECK in place, so the table is rebuilt as in V16.

CREATE TABLE device_meta_v39 (
    hostname                TEXT    NOT NULL,
    scope                   TEXT    NOT NULL CHECK (scope IN ('node', 'eye', 'hub', 'probe')),
    entity                  TEXT    NOT NULL,
    name                    TEXT,
    description             TEXT,
    created_at              TEXT    NOT NULL DEFAULT (datetime('now')),
    added_by                TEXT,
    offline_k_override      INTEGER,
    backfill_auto           INTEGER NOT NULL DEFAULT 1,
    store_interval_override INTEGER,
    serial                  TEXT,
    location                TEXT,

    PRIMARY KEY (hostname, scope, entity)
);

INSERT INTO device_meta_v39
    (hostname, scope, entity, name, description, created_at, added_by,
     offline_k_override, backfill_auto, store_interval_override, serial)
SELECT
    hostname, scope, entity, name, description, created_at, added_by,
    offline_k_override, backfill_auto, store_interval_override, serial
FROM device_meta;

DROP TABLE device_meta;

ALTER TABLE device_meta_v39 RENAME TO device_meta;
