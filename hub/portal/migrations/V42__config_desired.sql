-- The Portal's own stored copy of every desired Hub setting that has no
-- other home (plan/portal/config_sync.md, amendment 2026-10-15): display
-- lines, screen/LED/buzzer, sensor names and locations, intervals, the Hub
-- label. Bounds stay in `field_thresholds` (decision #41).
--
-- One row per document key (`hub/led_brightness`, `hub/sensor_name/3`),
-- holding the verb and its params as relayed; the sync item is built from
-- it verbatim. `seeded_at` marks a Hub whose missing keys were filled once
-- from its retained config/state, so the first sync does not wipe it.
CREATE TABLE config_desired (
    hub_id     TEXT    NOT NULL,
    key        TEXT    NOT NULL,
    command    TEXT    NOT NULL,
    params     TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (hub_id, key)
);

ALTER TABLE config_generation ADD COLUMN seeded_at INTEGER;
