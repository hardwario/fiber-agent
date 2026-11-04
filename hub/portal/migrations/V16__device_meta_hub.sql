-- The Hub's operator name and description (plan/portal/devices.hub.md §3.1
-- "Název a popis", §7 and §12 D — the portal half of B0's `device_meta`).
--
-- Two things V9 does not allow, and the same form needs both.
--
-- **1. `scope = 'hub'` does not fit through V9's CHECK.** §3.1 records
-- OQ 7 as "V9 vznikne rovnou se scope ('node','eye','hub')", but V9 shipped
-- with `CHECK (scope IN ('node', 'eye'))` — the decision landed after the
-- migration. V9's header sentence about scopes being "by convention, not by
-- constraint" is about the scope-SPECIFIC COLUMNS (`backfill_auto` is a Node
-- concept, `store_interval_override` a Beacon one), not about the `scope`
-- value itself, which is constrained. Inserting a Hub row against V9 fails
-- with `CHECK constraint failed`. SQLite cannot widen a CHECK in place, so
-- this is the table rebuild — the only way, and the reason a one-column
-- `ALTER TABLE` was not enough.
--
-- **2. There is no `name` column.** §3.1 puts the Hub's operator name
-- portal-side on purpose: `device_label` travels in the device config, which
-- a factory reset wipes, so the name an operator typed would vanish with it.
-- Nullable and not backfilled: a Hub that has never been named reads as
-- NULL, and the read side falls back to `device_label` and then to the
-- hostname rather than inventing one.
--
-- The column lands on `device_meta` rather than in a Hub-only table because
-- it is the same fact for every device class — Nodes and Beacons get the
-- column for free and may start using it without another migration.
--
-- **The number is deliberate**, for the reason V14's header spells out at
-- length: refinery runs with `abort_missing`, so a file numbered at or below
-- the highest applied version is a hard failure on every subsequent start
-- (devices.backend.md §21.1, where V10 was permanently burned). This file was
-- written as V15 on `dev-michal`; the MQTT rebuild landed its own V15
-- (`service_device_keys`) on `main` first, so at the merge (2026-09-16) this
-- one became V16 — the next safe number, no hole. A dev database that applied
-- the V15 spelling must be recreated: refinery validates the applied checksum.
--
-- `device_meta` carries no index, view or trigger, and nothing references it
-- by foreign key, so the rebuild is the three statements below and nothing
-- else has to be recreated. `field_thresholds` (V4) carries the same
-- two-value CHECK and is left alone: a Hub's wired-line thresholds live on
-- the device, not here, so nothing writes a `'hub'` threshold row.

CREATE TABLE device_meta_v16 (
    hostname                TEXT    NOT NULL,
    scope                   TEXT    NOT NULL CHECK (scope IN ('node', 'eye', 'hub')),
    entity                  TEXT    NOT NULL,

    -- Operator-supplied display name. NULL = never named; the reader falls
    -- back to `device_label` and then to the hostname (devices.hub.md §3.1).
    name                    TEXT,

    description             TEXT,
    created_at              TEXT    NOT NULL DEFAULT (datetime('now')),
    added_by                TEXT,
    offline_k_override      INTEGER,
    backfill_auto           INTEGER NOT NULL DEFAULT 1,
    store_interval_override INTEGER,
    serial                  TEXT,

    PRIMARY KEY (hostname, scope, entity)
);

INSERT INTO device_meta_v16
    (hostname, scope, entity, name, description, created_at, added_by,
     offline_k_override, backfill_auto, store_interval_override, serial)
SELECT
    hostname, scope, entity, NULL, description, created_at, added_by,
    offline_k_override, backfill_auto, store_interval_override, serial
FROM device_meta;

DROP TABLE device_meta;

ALTER TABLE device_meta_v16 RENAME TO device_meta;
