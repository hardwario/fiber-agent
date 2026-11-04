-- An admin deletes a calibration certificate (GitLab #99,
-- plan/portal/calibration.cs.md amendment 2026-10-13).
--
-- The PDF goes; the row stays as the record of that deletion, so the history
-- still lists it with who deleted it and why. `deleted_at` is unix seconds
-- like `uploaded_at`, `deleted_by` the username (denormalised like
-- `uploaded_by`), `delete_message` the reason the form requires. The three
-- are set together or not at all. The current calibration of a device is now
-- its newest row that is not deleted; a deleted row is never current, never
-- counted against the per-device cap and never served for download.
--
-- Plain `ADD COLUMN`s, no table rebuild: V35 already shipped to bench Hubs,
-- so the deletion columns land as their own step and existing rows keep
-- NULL in all three (= not deleted). The all-or-none CHECK rides on the last
-- column; SQLite lets a column CHECK name sibling columns and checks it
-- against the existing rows when the column is added, which all-NULL rows
-- pass.
ALTER TABLE device_calibrations ADD COLUMN deleted_at INTEGER;
ALTER TABLE device_calibrations ADD COLUMN deleted_by TEXT;
ALTER TABLE device_calibrations ADD COLUMN delete_message TEXT
    CHECK ((deleted_at IS NULL) = (deleted_by IS NULL)
       AND (deleted_at IS NULL) = (delete_message IS NULL));
