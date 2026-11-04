-- M6-B: alarm lifecycle + notification persistence.
-- Shapes per plan/v1/viewer/rest.md ("Alarms", "Global email / webhook
-- reports", "Per-device reports", "System alerts") and ws.md ("History
-- logging").
--
-- Crypto note: all *_enc columns hold blobs sealed by the bin's KeyService
-- (AES-256-GCM, nonce-prefixed) — this schema and the storage crate never
-- see plaintext secrets (same contract as V3 device keys).

-- alarm_events grows two columns:
--   sensor_location — v1 AlarmEventDB.sensor_location (display metadata);
--   native_key      — device-side idempotency key for fPort-3 alarm frames,
--                     unique when present so a re-delivered frame cannot
--                     create a second row (NULL for viewer-derived alarms).
ALTER TABLE alarm_events ADD COLUMN sensor_location TEXT;
ALTER TABLE alarm_events ADD COLUMN native_key TEXT;

CREATE UNIQUE INDEX idx_alarm_events_native_key
    ON alarm_events (native_key) WHERE native_key IS NOT NULL;

-- Per-alarm operator notes (rest.md AlarmNoteDB). Acknowledge reasons are
-- stored here too. Rows die with their alarm (ON DELETE CASCADE).
CREATE TABLE alarm_notes (
    id          TEXT PRIMARY KEY,
    alarm_id    TEXT NOT NULL REFERENCES alarm_events(id) ON DELETE CASCADE,
    user_id     INTEGER,
    username    TEXT NOT NULL,
    content     TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_alarm_notes_alarm_id ON alarm_notes (alarm_id);

-- Global email/webhook delivery config (rest.md GlobalEmailConfig) —
-- singleton row (id = 1), seeded below so reads never miss. Secrets are
-- write-only at the API: responses only report *_set flags, the enc blobs
-- never leave the server.
CREATE TABLE notify_global_config (
    id                    INTEGER PRIMARY KEY CHECK (id = 1),
    provider              TEXT    NOT NULL DEFAULT 'none'
                          CHECK (provider IN ('none', 'sendgrid', 'smtp')),
    from_email            TEXT,
    from_name             TEXT    NOT NULL DEFAULT 'ProXimos Monitor',
    sendgrid_api_key_enc  BLOB,
    smtp_host             TEXT,
    smtp_port             INTEGER,
    smtp_username         TEXT,
    smtp_password_enc     BLOB,
    smtp_use_tls          INTEGER NOT NULL DEFAULT 1,
    smtp_use_ssl          INTEGER NOT NULL DEFAULT 0,
    webhook_enabled       INTEGER NOT NULL DEFAULT 0,
    webhook_url           TEXT,
    webhook_method        TEXT    NOT NULL DEFAULT 'POST',
    webhook_headers_enc   BLOB,
    webhook_timeout_s     INTEGER NOT NULL DEFAULT 10,
    webhook_max_attempts  INTEGER NOT NULL DEFAULT 3,
    updated_at            TEXT
);

INSERT INTO notify_global_config (id) VALUES (1);

-- Per-device report recipients + event toggles (rest.md "Per-device
-- reports"). `recipients` is a JSON array of email addresses.
CREATE TABLE device_report_config (
    hostname         TEXT PRIMARY KEY,
    recipients       TEXT    NOT NULL DEFAULT '[]',
    enabled          INTEGER NOT NULL DEFAULT 0,
    notify_on_alarm  INTEGER NOT NULL DEFAULT 1,
    notify_on_clear  INTEGER NOT NULL DEFAULT 0,
    notify_on_ack    INTEGER NOT NULL DEFAULT 0,
    updated_at       TEXT
);

-- System alert thresholds/toggles (rest.md "System alerts") — singleton
-- row (id = 1), seeded below with the documented defaults.
CREATE TABLE system_alert_config (
    id                        INTEGER PRIMARY KEY CHECK (id = 1),
    recipients                TEXT    NOT NULL DEFAULT '[]',
    battery_enabled           INTEGER NOT NULL DEFAULT 1,
    battery_threshold         INTEGER NOT NULL DEFAULT 20,
    network_enabled           INTEGER NOT NULL DEFAULT 1,
    storage_enabled           INTEGER NOT NULL DEFAULT 1,
    storage_threshold         INTEGER NOT NULL DEFAULT 90,
    device_offline_enabled    INTEGER NOT NULL DEFAULT 1,
    version_mismatch_enabled  INTEGER NOT NULL DEFAULT 0,
    expected_version          TEXT,
    updated_at                TEXT
);

INSERT INTO system_alert_config (id) VALUES (1);

-- Unified delivery history. v1 kept two tables (report_history +
-- system_alert_history, ws.md "History logging") and merged them in Python
-- for GET /api/reports/history; v2 stores both in one table with a `source`
-- discriminator so that endpoint is a single filtered query. Report rows
-- use `subject`, system-alert rows use `message` (v1 shape difference kept
-- as two nullable columns). Webhook deliveries are report rows with
-- recipients '[]' and provider 'webhook'.
CREATE TABLE report_history (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    hostname       TEXT,
    event_type     TEXT    NOT NULL,
    recipients     TEXT    NOT NULL DEFAULT '[]',
    subject        TEXT,
    message        TEXT,
    status         TEXT    NOT NULL CHECK (status IN ('sent', 'failed')),
    error_message  TEXT,
    provider       TEXT,
    source         TEXT    NOT NULL CHECK (source IN ('report', 'system_alert')),
    sent_at        TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_report_history_sent_at ON report_history (sent_at);
