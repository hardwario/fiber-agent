-- The alarm-slot layout the portal last wrote to a Node (plan/portal/nodes_phase1.cs.md WP2).
--
-- The portal is the source of truth for a Node's on-device alarm rules and reads none back:
-- slot `i` (`alarms.alarm_i`) belongs to the i-th quantity, in the Node's order of active
-- quantities it can watch, and every write replaces the whole table. When the Node's active
-- sensors change (a new config info after a boot), that order moves, and the rules on the Node
-- sit in the wrong slots until the table is sent again. This row is what the portal compares
-- against to notice.
--
-- `layout` is a JSON array of field names in slot order. One row per Node; the removal route
-- deletes it with the Node's other portal-side state.
CREATE TABLE node_alarm_layout (
    hostname TEXT    NOT NULL,
    node_key TEXT    NOT NULL,
    layout   TEXT    NOT NULL,
    sent_at  INTEGER NOT NULL,
    PRIMARY KEY (hostname, node_key)
);
