-- Per-device operator metadata and per-device overrides for Nodes and
-- Beacons (plan/portal/devices.backend.md §3, WP0.2 — B0's foundation).
--
-- There was nowhere to put any of this. V1-V8 hold readings, thresholds,
-- jobs, groups and auth; a device's identity lives only in the in-memory
-- `NodeState`/`BeaconState`, which is rebuilt from retained MQTT on every
-- start and therefore cannot carry anything an operator typed. Every
-- per-device override the plan's later stages need -- a description, an
-- offline-detection multiplier, an auto-backfill toggle, a store interval --
-- lands here.
--
-- Keyed like `field_thresholds` (V4) deliberately, one rung shallower: that
-- table is per (hostname, scope, entity, FIELD), this one is per device, so
-- the natural key is the same minus `field`. `scope` carries the same two
-- values and the same CHECK, and `entity` the same convention -- a Node's
-- lowercase node_key, a Beacon's uppercase MAC.
--
-- Nullable-means-inherit is the rule for the two override columns, and it is
-- load-bearing: `offline_k_override` NULL takes the YAML default
-- (`nodes.offline_detector` / `beacons.offline_detector`), while an explicit
-- 0 DISABLES detection for this one device. That distinction is why the
-- column is nullable rather than defaulted -- 0 is a real setting, not a
-- missing one. `store_interval_override` reads the same way against
-- `beacons.store_interval`.
--
-- `backfill_auto` is NOT NULL with a default instead, because "inherit" has
-- no meaning for it: there is no global auto-backfill switch, only this
-- per-Node one, and it blocks the automatic path alone -- a manual job from
-- the detail page always runs (§6, B3).
--
-- Columns are scope-specific by convention, not by constraint: `backfill_auto`
-- is a Node concept and `store_interval_override` a Beacon one. A CHECK
-- pinning each to its scope was considered and dropped -- it would have to be
-- rewritten the first time a column becomes meaningful for both, and the two
-- readers already select by scope.

CREATE TABLE device_meta (
    hostname                TEXT    NOT NULL,
    scope                   TEXT    NOT NULL CHECK (scope IN ('node', 'eye')),
    entity                  TEXT    NOT NULL,

    -- Operator-supplied free text, shown on the list row and matched by the
    -- fulltext filter (§4, B1).
    description             TEXT,

    -- When the operator registered this device, which is the list's default
    -- sort (newest first). Distinct from `NodeState::added_at`, the store's
    -- own write-once first-seen: that one resets with the process.
    created_at              TEXT    NOT NULL DEFAULT (datetime('now')),
    added_by                TEXT,

    -- NULL = inherit the YAML default; 0 = offline detection off for this
    -- device. See the header.
    offline_k_override      INTEGER,

    -- Node only. Blocks auto-backfill; manual jobs are unaffected.
    backfill_auto           INTEGER NOT NULL DEFAULT 1,

    -- Beacon only. NULL = inherit `beacons.store_interval` (minutes).
    store_interval_override INTEGER,

    PRIMARY KEY (hostname, scope, entity)
);
