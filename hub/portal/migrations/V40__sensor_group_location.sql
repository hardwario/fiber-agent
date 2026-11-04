-- A sensor group carries a location like every other entity's metadata
-- (monitoring.md, dodatek 2026-10-15): free text, the same combobox and
-- suggestions as the metadata card. NULL = none set.
--
-- V40 (once reserved by the config sync design, now on hynek/config-sync).
ALTER TABLE sensor_groups ADD COLUMN location TEXT;
