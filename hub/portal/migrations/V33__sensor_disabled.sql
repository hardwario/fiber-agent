-- Switched-off sensors (GitLab #120, plan/portal/devices.node.md amendment 2026-10-13).
--
-- A row means the operator switched this one sensor of a Node or Beacon off:
-- its values are not stored, graded or shown outside the sensor settings table.
-- No row = on. Switching off deletes the sensor's history in the same
-- transaction (portal-storage `disable_sensor`); switching on deletes the row.
CREATE TABLE sensor_disabled (
    hostname    TEXT NOT NULL,
    scope       TEXT NOT NULL CHECK (scope IN ('node', 'eye')),
    entity      TEXT NOT NULL,
    field       TEXT NOT NULL,
    disabled_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    actor       TEXT,
    PRIMARY KEY (hostname, scope, entity, field)
);
