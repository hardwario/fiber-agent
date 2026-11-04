-- M5-B: Node (STICKER) + Beacon (EYE) persistence.
-- Row semantics per plan/v1/viewer/mqtt.md ("lorawan/sensors" persistence
-- gated on last_seen change; "lorawan/sensors/{dev_eui}/history" replay
-- frames written dedup-safe on the natural key; "eye/sensors" live readings
-- throttled to 30 s buckets with a synthetic message_id; "export/eye"
-- deduped by device message_id) and ws.md ("backfill_job" job_response
-- fields + status enum).

-- Per-field Node telemetry, one row per (field, uplink ts). History replay
-- frames re-insert the same natural key harmlessly (INSERT OR IGNORE).
CREATE TABLE node_field_readings (
    hostname    TEXT    NOT NULL,
    dev_eui     TEXT    NOT NULL,
    field       TEXT    NOT NULL,
    ts          INTEGER NOT NULL,
    value       REAL,
    alarm_state TEXT,
    source      TEXT    NOT NULL DEFAULT 'live',
    PRIMARY KEY (hostname, dev_eui, field, ts)
);

CREATE TABLE node_counter_readings (
    hostname TEXT    NOT NULL,
    dev_eui  TEXT    NOT NULL,
    counter  TEXT    NOT NULL,
    ts       INTEGER NOT NULL,
    value    INTEGER,
    PRIMARY KEY (hostname, dev_eui, counter, ts)
);

CREATE TABLE node_events (
    hostname   TEXT    NOT NULL,
    dev_eui    TEXT    NOT NULL,
    event_type TEXT    NOT NULL,
    ts         INTEGER NOT NULL,
    payload    TEXT,
    PRIMARY KEY (hostname, dev_eui, event_type, ts)
);

-- Ring buffer of the last 10 raw uplink payloads per Node (mqtt.md: "recent
-- raw messages (last 10/Node)"); the repo trims older rows on every insert.
CREATE TABLE node_raw_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    hostname    TEXT    NOT NULL,
    dev_eui     TEXT    NOT NULL,
    received_at INTEGER NOT NULL,
    payload     TEXT    NOT NULL
);

CREATE INDEX idx_node_raw_messages_node
    ON node_raw_messages (hostname, dev_eui, id);

-- EYE/BLE beacon readings. Live snapshots arrive with a synthetic
-- "{mac}-live-{ts//30}" message_id (30 s bucket throttle + multi-gateway
-- dedup); export/eye rows carry the device's message_id. The partial unique
-- index makes both paths dedup-safe beyond the natural key.
CREATE TABLE eye_readings (
    hostname      TEXT    NOT NULL,
    mac           TEXT    NOT NULL,
    ts            INTEGER NOT NULL,
    temperature_c REAL,
    humidity_pct  INTEGER,
    event_type    TEXT    NOT NULL,
    message_id    TEXT,
    PRIMARY KEY (hostname, mac, ts, event_type)
);

CREATE UNIQUE INDEX idx_eye_readings_message_id
    ON eye_readings (message_id) WHERE message_id IS NOT NULL;

-- Per-field alarm thresholds for Nodes (entity = dev_eui, lowercase) and
-- EYE beacons (entity = MAC, uppercase). History rows are written only when
-- the four threshold values actually change (actor e.g. a username or
-- 'firmware-echo' for device-authoritative reconciliation).
CREATE TABLE field_thresholds (
    hostname      TEXT    NOT NULL,
    scope         TEXT    NOT NULL CHECK (scope IN ('node', 'eye')),
    entity        TEXT    NOT NULL,
    field         TEXT    NOT NULL,
    critical_low  REAL,
    warning_low   REAL,
    warning_high  REAL,
    critical_high REAL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    updated_at    TEXT,
    PRIMARY KEY (hostname, scope, entity, field)
);

CREATE TABLE field_threshold_history (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    hostname      TEXT NOT NULL,
    scope         TEXT NOT NULL,
    entity        TEXT NOT NULL,
    field         TEXT NOT NULL,
    critical_low  REAL,
    warning_low   REAL,
    warning_high  REAL,
    critical_high REAL,
    actor         TEXT,
    changed_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_field_threshold_history_entity
    ON field_threshold_history (hostname, scope, entity, field, id);

-- Node history backfill jobs (ws.md "backfill_job"). Active =
-- pending/requested/receiving; only requested/receiving can be credited
-- with frames; 'pending' is the only freely-cancellable state.
CREATE TABLE backfill_jobs (
    job_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    hostname         TEXT    NOT NULL,
    dev_eui          TEXT    NOT NULL,
    status           TEXT    NOT NULL CHECK (status IN
        ('pending', 'requested', 'receiving', 'done', 'failed', 'cancelled')),
    time_from        INTEGER NOT NULL,
    time_to          INTEGER NOT NULL,
    "trigger"        TEXT    NOT NULL,
    samples_expected INTEGER,
    samples_received INTEGER NOT NULL DEFAULT 0,
    retry_count      INTEGER NOT NULL DEFAULT 0,
    error_message    TEXT,
    device_refused   INTEGER NOT NULL DEFAULT 0,
    requested_at     INTEGER,
    last_sample_at   INTEGER,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE INDEX idx_backfill_jobs_dev_eui ON backfill_jobs (dev_eui, job_id);
CREATE INDEX idx_backfill_jobs_host_status ON backfill_jobs (hostname, status);
