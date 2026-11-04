-- STICKER v1.5.0 renames the 1-Wire tilt field `w1_N_tilt_alert` to `w1_N_tilt`
-- (plan/control/radio/node-firmware-v1.5.0.md §1.1, issue #126 item 3). Everything that keys
-- portal-side configuration on the field name moves to the new name, so a monitoring card, a
-- threshold row or a draft keeps pointing at the field the Node reports:
--
-- * `field_thresholds`, `field_threshold_history`, `node_threshold_delivery` (`field`);
-- * `pending_config` threshold drafts (`param_key` = `threshold.{field}.{bound}`);
-- * `sensor_group_members.fields` and `node_alarm_layout.layout` (JSON arrays of field names).
--
-- Where a row under the new name already exists (a v1.5.0 Node's own), it wins and the old row
-- goes. Stored readings (`node_field_readings`) and closed alarms are history and keep the old
-- name: fiber-protocol keeps `w1_N_tilt_alert` in its registry, so the old series still renders.
-- N is the 1-Wire slot (1..4); the pattern takes any single digit.

UPDATE OR IGNORE field_thresholds
SET field = substr(field, 1, length(field) - 6)
WHERE scope = 'node' AND field GLOB 'w1_[0-9]_tilt_alert';
DELETE FROM field_thresholds
WHERE scope = 'node' AND field GLOB 'w1_[0-9]_tilt_alert';

UPDATE field_threshold_history
SET field = substr(field, 1, length(field) - 6)
WHERE scope = 'node' AND field GLOB 'w1_[0-9]_tilt_alert';

UPDATE OR IGNORE node_threshold_delivery
SET field = substr(field, 1, length(field) - 6)
WHERE field GLOB 'w1_[0-9]_tilt_alert';
DELETE FROM node_threshold_delivery
WHERE field GLOB 'w1_[0-9]_tilt_alert';

-- `threshold.w1_1_tilt_alert.critical_low` -> `threshold.w1_1_tilt.critical_low`.
UPDATE OR IGNORE pending_config
SET param_key = replace(param_key, '_tilt_alert.', '_tilt.')
WHERE param_key GLOB 'threshold.w1_[0-9]_tilt_alert.*';
DELETE FROM pending_config
WHERE param_key GLOB 'threshold.w1_[0-9]_tilt_alert.*';

-- JSON arrays: rename in place, keep the order, drop a duplicate the rename would create.
UPDATE sensor_group_members
SET fields = (
    SELECT json_group_array(name) FROM (
        SELECT name, min(pos) AS pos FROM (
            SELECT CASE WHEN value GLOB 'w1_[0-9]_tilt_alert'
                        THEN substr(value, 1, length(value) - 6) ELSE value END AS name,
                   CAST(key AS INTEGER) AS pos
            FROM json_each(sensor_group_members.fields)
        )
        GROUP BY name
        ORDER BY pos
    )
)
WHERE fields IS NOT NULL AND json_valid(fields) AND json_type(fields) = 'array'
  AND EXISTS (SELECT 1 FROM json_each(sensor_group_members.fields)
              WHERE value GLOB 'w1_[0-9]_tilt_alert');

UPDATE node_alarm_layout
SET layout = (
    SELECT json_group_array(name) FROM (
        SELECT name, min(pos) AS pos FROM (
            SELECT CASE WHEN value GLOB 'w1_[0-9]_tilt_alert'
                        THEN substr(value, 1, length(value) - 6) ELSE value END AS name,
                   CAST(key AS INTEGER) AS pos
            FROM json_each(node_alarm_layout.layout)
        )
        GROUP BY name
        ORDER BY pos
    )
)
WHERE json_valid(layout) AND json_type(layout) = 'array'
  AND EXISTS (SELECT 1 FROM json_each(node_alarm_layout.layout)
              WHERE value GLOB 'w1_[0-9]_tilt_alert');
