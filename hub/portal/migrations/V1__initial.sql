-- M1-P: hot-tier readings, alarm events, export-ledger dedup.
-- Row semantics per plan/v1/viewer/mqtt.md (probe_1m upsert authoritative
-- 30-day hot tier; replay source survives later live sweeps) and
-- plan/v1/viewer/rest.md (AlarmEventDB shape).

CREATE TABLE readings_minute (
    hostname          TEXT    NOT NULL,
    sensor_line       INTEGER NOT NULL,
    minute_ts         INTEGER NOT NULL,
    min_c             REAL,
    avg_c             REAL,
    max_c             REAL,
    sample_count      INTEGER,
    disconnect_count  INTEGER,
    worst_alarm       TEXT,
    source            TEXT    NOT NULL DEFAULT 'live',
    data_hmac         BLOB,
    PRIMARY KEY (hostname, sensor_line, minute_ts)
);

CREATE TABLE readings_raw (
    hostname     TEXT    NOT NULL,
    sensor_line  INTEGER NOT NULL,
    ts           INTEGER NOT NULL,
    min_c        REAL,
    avg_c        REAL,
    max_c        REAL,
    sample_count INTEGER,
    PRIMARY KEY (hostname, sensor_line, ts)
);

CREATE TABLE alarm_events (
    id                        TEXT PRIMARY KEY,
    hostname                  TEXT NOT NULL,
    sensor_id                 TEXT NOT NULL,
    sensor_name               TEXT,
    alarm_type                TEXT NOT NULL,
    severity                  TEXT NOT NULL,
    from_state                TEXT,
    to_state                  TEXT,
    message                   TEXT,
    value                     REAL,
    threshold                 REAL,
    triggered_at              TEXT NOT NULL,
    cleared_at                TEXT,
    acknowledged_at           TEXT,
    acknowledged_by_user_id   INTEGER,
    acknowledged_by_username  TEXT,
    created_at                TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_alarm_events_hostname_triggered_at
    ON alarm_events (hostname, triggered_at);

CREATE TABLE export_ledger (
    message_id   TEXT PRIMARY KEY,
    stream       TEXT NOT NULL,
    exported_at  INTEGER,
    received_at  INTEGER NOT NULL
);
