-- Bookkeeping for operator-initiated database maintenance (full VACUUM).
--
-- Routine page reclaim is incremental and needs no state: the retention sweep
-- calls `PRAGMA incremental_vacuum` every pass. A *full* VACUUM is different --
-- it locks out every writer for its whole duration, and on this device the
-- blocked writer is the alarm engine, so it is only ever run because an admin
-- asked for it. This table is what the settings page reads to show whether one
-- is running and how the last one went.
--
-- Singleton, same shape as notify_global_config in V5.
CREATE TABLE maintenance_state (
    id                     INTEGER PRIMARY KEY CHECK (id = 1),

    -- Unix seconds of the last successful full VACUUM, NULL if never run.
    last_full_vacuum_at    INTEGER,
    -- Wall-clock duration of that run, so an operator can judge the cost
    -- before asking for another one. There is no such figure for this
    -- hardware yet -- see plan/portal/status.md.
    last_full_vacuum_ms    INTEGER,
    -- Error text of the last attempt, NULL when it succeeded. Kept rather
    -- than only logged: journald is not where an operator looks.
    last_full_vacuum_error TEXT,

    -- Set for the duration of a run so a second request is refused instead of
    -- queued. Cleared on completion; see the startup reconciliation in
    -- src/retention.rs for the crash case.
    full_vacuum_running    INTEGER NOT NULL DEFAULT 0,

    -- Databases created before V7 have auto_vacuum=NONE baked in (it can only
    -- be set while the file is empty), which makes incremental_vacuum a silent
    -- no-op. Converting needs one full VACUUM, so it is offered to the operator
    -- rather than done behind their back.
    auto_vacuum_converted  INTEGER NOT NULL DEFAULT 0
);

INSERT INTO maintenance_state (id) VALUES (1);
