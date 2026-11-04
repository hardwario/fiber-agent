-- config_sync.md §3 / §13 Q10–Q11, Q20c: the 4 h delivery deadline of a
-- Node's pending configuration. One row per Node while its message waits
-- for the Node's confirmation; "Send again" restarts the deadline. Deleted
-- once the Node confirms (no draft row left).
CREATE TABLE node_config_deadline (
    hostname    TEXT NOT NULL COLLATE NOCASE,
    node_key    TEXT NOT NULL,
    sent_at     INTEGER NOT NULL,
    deadline_at INTEGER NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (hostname, node_key)
);
