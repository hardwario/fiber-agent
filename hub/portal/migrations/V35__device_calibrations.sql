-- Calibration certificates of Nodes and Beacons (plan/portal/calibration.cs.md
-- §4, GitLab #99).
--
-- One row per uploaded certificate. The PDF itself is NOT in the database: it
-- lives at `{data_dir}/calibrations/{scope}/{entity}/{id}.pdf`, inside the
-- directory the Landlock ruleset already grants (`src/sandbox.rs`), and this
-- row is what names it. Keeping multi-megabyte blobs out of SQLite keeps them
-- out of every `VACUUM` rewrite and out of the spill journal's reach.
--
-- The **current** calibration of a device is its newest row (`uploaded_at`,
-- then `rowid`, which breaks the tie two uploads in one second would leave).
-- Older rows are the history the card lists; nothing prunes them, and
-- removing the device does not either — like its measurement history
-- (§4, "Odebrání zařízení kalibrace nemaže").
--
-- `id` is a random UUID (32 hex digits, no dashes) rather than an integer:
-- the file is written and renamed into place *before* the row is inserted,
-- so the name has to exist before SQLite hands out a rowid. A crash between
-- the two leaves an orphan file and no row, which is harmless, where the
-- other order would leave a row whose download 404s.
--
-- `scope`/`entity` are `device_meta`'s identity pair (V9/V16): `node` + the
-- lowercase `node_key`, or `eye` + the uppercase MAC. `hub` is left out of
-- the CHECK on purpose — Hub probes are out of scope (§5), and widening the
-- CHECK later is a table rebuild, as V16 shows.
--
-- `valid_until` is the last day the certificate is valid, stored as unix
-- seconds of that day's UTC midnight — an integer like every other newer
-- timestamp here (V12's `ts`, V24's `sent_at`), so the day arithmetic the
-- reminder needs is a division and not a string parse.
--
-- `uploaded_by` is the username, denormalised the way V12's `author` is, so
-- the card still says who uploaded a certificate after that user is deleted.
--
-- Written as V18 on the branch while !99 held V17 there; renumbered to V35
-- when the branch was rebased onto `main`, after V33 sensor_disabled (!166)
-- and V34 eye_readings.source_hub (!167) (plan/portal/status.md, 2026-10-13).
-- A bench database that applied the branch's V18 does not open on this tree
-- (refinery's `abort_divergent`) and is recreated.
CREATE TABLE device_calibrations (
    id          TEXT    PRIMARY KEY,
    hostname    TEXT    NOT NULL,
    scope       TEXT    NOT NULL CHECK (scope IN ('node', 'eye')),
    entity      TEXT    NOT NULL,
    valid_until INTEGER NOT NULL,
    file_name   TEXT    NOT NULL,
    file_size   INTEGER NOT NULL CHECK (file_size > 0),
    sha256      TEXT    NOT NULL,
    note        TEXT,
    uploaded_by TEXT    NOT NULL,
    uploaded_at INTEGER NOT NULL
);

-- Both reads this table serves walk one device's rows newest first: the
-- card's list, and the "newest row per device" the reminder sweep takes.
CREATE INDEX idx_device_calibrations_device
    ON device_calibrations (hostname, scope, entity, uploaded_at);
