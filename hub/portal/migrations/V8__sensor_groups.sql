-- Sensor groups: the operator's own grouping of probes, Nodes and Beacons,
-- which is what /monitoring renders. Ported from v1's `groups_repository.py`
-- (its v4 layout, arrived at through three in-place migrations there) with
-- one rename: `dev_eui` -> `node_key`, matching the V6 rename of the Node
-- identity everywhere else in this schema.
--
-- Membership is per ENTITY, not per value: a member names one probe line, one
-- Node or one Beacon. What the UI draws from that is a *card* per field, so a
-- Node with three reporting fields is three cards inside one row --
-- `render::group_rows_ctx` does that expansion, the schema does not.

CREATE TABLE sensor_groups (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL,
    description TEXT,
    -- Reserved for operator-ordered groups; nothing writes a non-zero value
    -- yet, so the page falls back to sorting by name (v1 does the same).
    sort_order  INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE sensor_group_members (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id    INTEGER NOT NULL REFERENCES sensor_groups(id) ON DELETE CASCADE,
    hostname    TEXT    NOT NULL,

    -- Exactly one of the three is set -- that is what the member IS. The
    -- CHECK is the only thing keeping a malformed row out, so keep it.
    sensor_line INTEGER,
    node_key    TEXT,
    mac         TEXT,

    -- JSON array of field names to show for this member, or NULL for "every
    -- field it reports that is continuous and thresholdable, minus battery".
    -- Only meaningful for Node/Beacon members: a wired probe has exactly one
    -- value and ignores this column.
    fields      TEXT,

    sort_order  INTEGER NOT NULL DEFAULT 0,

    CHECK (
        (sensor_line IS NOT NULL) + (node_key IS NOT NULL) + (mac IS NOT NULL) = 1
    )
);

-- The same entity cannot be added to one group twice. COALESCE because SQLite
-- treats NULLs as distinct in a unique index, which would let a probe be added
-- repeatedly (its node_key/mac are both NULL every time).
CREATE UNIQUE INDEX sensor_group_members_unique ON sensor_group_members (
    group_id,
    hostname,
    COALESCE(sensor_line, -1),
    COALESCE(node_key, ''),
    COALESCE(mac, '')
);

CREATE INDEX idx_group_members_group ON sensor_group_members (group_id);

-- Which of the four monitoring layouts this operator last chose. On the
-- account rather than in the browser, as in v1 (`user.monitoring_view_mode`),
-- so the setting follows them between the workstation and the panel on the
-- wall. localStorage stays the pre-login cache only.
ALTER TABLE users ADD COLUMN monitoring_view_mode TEXT NOT NULL
    DEFAULT 'table-simplified'
    CHECK (monitoring_view_mode IN (
        'card-detailed', 'card-simplified', 'table-detailed', 'table-simplified'
    ));
