-- M8: rename the viewer's internal Node identity column from `dev_eui` to
-- `node_key`. A P2P STICKER (plan/viewer/p2p_nodes.md §1, plan/decisions.md
-- #7/#8) is the same Node entity but has no dev_eui — it is identified by
-- serial_number — so the storage-layer identity column can no longer be
-- named after the LoRaWAN-only wire concept. This is a rename only: for
-- today's LoRaWAN nodes the column's VALUES are unchanged (still the
-- lowercase dev_eui), so URLs, SSE event ids, and alarm sensor_id strings
-- are all unaffected.
ALTER TABLE node_field_readings RENAME COLUMN dev_eui TO node_key;
ALTER TABLE node_counter_readings RENAME COLUMN dev_eui TO node_key;
ALTER TABLE node_events RENAME COLUMN dev_eui TO node_key;
ALTER TABLE node_raw_messages RENAME COLUMN dev_eui TO node_key;
ALTER TABLE backfill_jobs RENAME COLUMN dev_eui TO node_key;

-- SQLite cannot rename an index in place; drop + recreate under the new name.
DROP INDEX idx_backfill_jobs_dev_eui;
CREATE INDEX idx_backfill_jobs_node_key ON backfill_jobs (node_key, job_id);
