-- Manual operator events on one probe (plan/portal/monitoring.backend.md §6
-- WP3.4, spec plan/portal/monitoring.md §6.5 — MB3).
--
-- The probe-detail window's event list merges three sources: the value-class
-- transitions and the offline/online pair, both `alarm_events` rows, and the
-- operator's own notes — "údržba čidla", "výměna baterií" — which have
-- nowhere to live today. This is that place.
--
-- ## Why V12, verified rather than assumed
--
-- V10 is permanently burned (see `V13__pending_config.sql`'s header for the
-- refinery `abort_missing` mechanics): B3 landed V11 while V10 was still
-- unwritten, so every database that has run since carries history 1-9 + 11
-- with a hole at 10, and a file dropped into that hole fails `Storage::open`
-- on all eight of the portal's connections.
--
-- V12 is *not* such a hole. Re-verified read-only on 2026-09-09, after B4
-- landed: `refinery_schema_history` in `portal/dev-data/portal.db` holds
-- exactly versions 1,2,3,4,5,6,7,8,9,11 — B4's V13 has never been applied
-- there, so 12 is above that database's highest applied version, where
-- refinery imposes no contiguity at all. A fresh database applies
-- 1-9,11,12,13 in order and is likewise fine. The one database that would
-- refuse this file is one already at V13 with V12 absent; the only such
-- database in existence was MB3's own throwaway walk-through copy under a
-- scratchpad directory, discarded rather than migrated.
--
-- The reservations from §7 and devices.backend.md §10 therefore stand
-- unchanged: V12 is this table, V13 is B4's `pending_config`, V14 is B6's
-- optional `device_meta.serial`.
--
-- ## Shape
--
-- The identity columns are `sensor_group_members`' three-way shape verbatim
-- (V8), down to the one-of `CHECK` — an event belongs to a **probe**, and a
-- probe is exactly what that encoding names. Reusing it means one resolver
-- (`monitoring::parse_token` -> `MemberRef`) serves membership and events
-- alike, and an event survives its probe moving between groups because it
-- was never attached to a group in the first place (§6.5).
--
-- `hostname` rides along for the same reason it does on every other table
-- here: a `sensor_line` is only unique under its Hub.
--
-- `ts` is the operator-chosen event time in unix seconds — backdating is
-- explicitly allowed (§6.5) — while `created_at` is when the row was
-- written. Both are needed: the list orders by `ts`, the audit trail cares
-- about `created_at`.
--
-- `text` is capped at 240 characters in the CHECK as well as in the form,
-- because the form is not the only thing that can POST here. The lower
-- bound is 1: an empty note carries no information and the route refuses it
-- before this ever fires.
--
-- `author` is the username, denormalised exactly as
-- `alarm_events.acknowledged_by_username` and `field_thresholds.actor` are,
-- and for the same reason — the window must still say who wrote the note
-- after that user is deleted. `author_user_id` is the soft link, nullable
-- and with **no** foreign key, so deleting a user never cascades an
-- operator's maintenance history away.
CREATE TABLE sensor_events (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    hostname       TEXT    NOT NULL,
    sensor_line    INTEGER,
    node_key       TEXT,
    mac            TEXT,
    ts             INTEGER NOT NULL,
    text           TEXT    NOT NULL CHECK (length(text) BETWEEN 1 AND 240),
    author         TEXT    NOT NULL,
    author_user_id INTEGER,
    created_at     INTEGER NOT NULL,

    -- Exactly one identity column, as in V8's `sensor_group_members`. It is
    -- the only thing keeping a row that names two probes (or none) out.
    CHECK (
        (sensor_line IS NOT NULL) + (node_key IS NOT NULL) + (mac IS NOT NULL) = 1
    )
);

-- The one query this table serves: "every event of THIS probe in
-- [from, to)". `COALESCE` in the index expression, matching V8's unique
-- index, because SQLite cannot use a plain column index for a predicate
-- written against the coalesced form — and the resolver hands all three
-- identity columns down every time, two of them NULL.
CREATE INDEX idx_sensor_events_probe ON sensor_events (
    hostname,
    COALESCE(sensor_line, -1),
    COALESCE(node_key, ''),
    COALESCE(mac, ''),
    ts
);
