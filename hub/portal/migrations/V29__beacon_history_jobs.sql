-- Beacon archive history jobs (plan/portal/beacon_history.md §4.1, §7).
--
-- A table of its own, not `backfill_jobs`: that one is keyed by the
-- lowercase Node key, its id is the portal's autoincrement and its states
-- are driven by the portal's own downlink loop. A Beacon's archive download
-- is run by the Hub (`control/ble`, §2.4: the Hub owns the retries); the
-- portal only asks for a window and records what the Hub reports on
-- `{prefix}/{hostname}/eye/history` (§2.2).
--
-- `id` is the portal's `bh-<ulid>`, sent as the request's `job` and echoed
-- in every progress message. A download the Hub started without one (CLI,
-- an older portal) arrives under the Hub's own id and gets a row with
-- `trigger = 'manual'` and `created_by` NULL.
--
-- `from_ts` NULL = "resume from what the Hub already holds" (§2.1); `to_ts`
-- NULL = "up to now". Both unix seconds.
--
-- `trigger` is `manual` or `auto-on-reconnect` — the Node's values, so the
-- shared jobs table words both the same (§7.1 amends §4.1's
-- `auto_reconnect`).
--
-- The last six columns are the integrity counts of §7 Q6: what the Hub
-- reports with `done` (`sessions`, `rejected_chunks`, `future_dropped`,
-- `crc_failed` — NULL until known, never a made-up 0) and the portal's own
-- cross-check of archive points against live readings (`compared`,
-- `disagreed`).
CREATE TABLE beacon_history_jobs (
    id              TEXT    PRIMARY KEY,
    hostname        TEXT    NOT NULL,
    mac             TEXT    NOT NULL,
    from_ts         INTEGER,
    to_ts           INTEGER,
    "trigger"       TEXT    NOT NULL,
    state           TEXT    NOT NULL CHECK (state IN
        ('pending', 'requested', 'receiving', 'done', 'failed', 'cancelled')),
    received        INTEGER NOT NULL DEFAULT 0,
    expected        INTEGER,
    retry           INTEGER NOT NULL DEFAULT 0,
    reason          TEXT,
    created_by      TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    sessions        INTEGER,
    rejected_chunks INTEGER,
    future_dropped  INTEGER,
    crc_failed      INTEGER,
    compared        INTEGER,
    disagreed       INTEGER
);

CREATE INDEX idx_beacon_history_jobs_mac ON beacon_history_jobs (mac, created_at);
CREATE INDEX idx_beacon_history_jobs_host_state ON beacon_history_jobs (hostname, state);
