-- Calibration certificates of the Hub's wired probes (GitLab #99,
-- plan/portal/calibration.cs.md amendment 2026-10-14).
--
-- A probe is keyed by its Hub and line: `scope = 'probe'`, `entity` the
-- wire line "0".."7". The portal receives no probe serial or 1-Wire ROM
-- code, and the thresholds (V32) key the line the same way.
--
-- V35's `CHECK (scope IN ('node', 'eye'))` cannot be widened in place, so
-- this is the table rebuild V16 did for `device_meta`: same columns
-- (V36's deletion columns included), wider CHECK, rows and index carried
-- over. Column order is V35's then V36's, so `SELECT *` copies verbatim.
CREATE TABLE device_calibrations_new (
    id             TEXT    PRIMARY KEY,
    hostname       TEXT    NOT NULL,
    scope          TEXT    NOT NULL CHECK (scope IN ('node', 'eye', 'probe')),
    entity         TEXT    NOT NULL,
    valid_until    INTEGER NOT NULL,
    file_name      TEXT    NOT NULL,
    file_size      INTEGER NOT NULL CHECK (file_size > 0),
    sha256         TEXT    NOT NULL,
    note           TEXT,
    uploaded_by    TEXT    NOT NULL,
    uploaded_at    INTEGER NOT NULL,
    deleted_at     INTEGER,
    deleted_by     TEXT,
    delete_message TEXT
        CHECK ((deleted_at IS NULL) = (deleted_by IS NULL)
           AND (deleted_at IS NULL) = (delete_message IS NULL))
);

-- The rowid is carried over too: the "newest row per device" tie-break
-- (V35's header) reads it.
INSERT INTO device_calibrations_new (rowid, id, hostname, scope, entity, valid_until,
    file_name, file_size, sha256, note, uploaded_by, uploaded_at,
    deleted_at, deleted_by, delete_message)
SELECT rowid, id, hostname, scope, entity, valid_until,
    file_name, file_size, sha256, note, uploaded_by, uploaded_at,
    deleted_at, deleted_by, delete_message
FROM device_calibrations;

DROP TABLE device_calibrations;
ALTER TABLE device_calibrations_new RENAME TO device_calibrations;

CREATE INDEX idx_device_calibrations_device
    ON device_calibrations (hostname, scope, entity, uploaded_at);
