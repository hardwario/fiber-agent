-- The Node pending-configuration draft (plan/portal/devices.backend.md §7
-- WP4.2, spec plan/portal/devices.node.md §3.5 — B4).
--
-- Nothing in the portal remembers a configuration edit today: every
-- threshold and every interval is fire-and-forget, one key at a time,
-- straight down the relay. devices.node.md §3.5 asks for the opposite —
-- an edit becomes a server-side draft that every user can see, that carries
-- who made it and when, that survives an error and a timeout, and that is
-- sent as one deliberate act rather than as a side effect of typing.
--
-- ## Why V13 and not V10
--
-- §10 of the backend plan claimed **V10** for this table. V10 is
-- permanently burned and this table takes V13 instead.
--
-- `portal-storage` runs refinery's default `Runner`
-- (`crates/portal-storage/src/lib.rs`, `embedded::migrations::runner()`),
-- and refinery 0.8.16 defaults `abort_missing: true`
-- (`refinery-core/src/runner.rs`, `Runner::default`). With that flag set,
-- `refinery_core::traits::verify_migrations` returns
-- `Kind::MissingVersion` for **any** on-disk migration whose version is at
-- or below the highest already applied and which is not itself in
-- `refinery_schema_history` — not just for a file that has gone missing.
--
-- B3 landed `V11__backfill_not_before.sql` while V10 was still unwritten,
-- so every database that has run since carries history 1-9 and 11 with a
-- hole at 10. Verified against the dev database on 2026-09-09:
-- `refinery_schema_history` in `portal/dev-data/portal.db` holds exactly
-- versions 1,2,3,4,5,6,7,8,9,11. Dropping a `V10__pending_config.sql` into
-- this directory would therefore make `Storage::open` fail on every one of
-- the portal's eight connections — the whole process, not one feature — the
-- first time anyone upgraded. `set_abort_missing(false)` is not the
-- workaround it looks like: it downgrades the check to a log line and
-- **skips** the migration, so the table would silently never be created.
--
-- Numbers above the highest applied one have no such constraint (refinery
-- does not require contiguity upward), so the reservations stand: V12 is
-- MB3's `sensor_events`, V13 is this table, V14 is B6's optional
-- `device_meta.serial`. Whichever of MB3/B4 lands first keeps its number.
--
-- ## Shape
--
-- Natural key `(hostname, node_key, param_key)` — one desired value per
-- key per Node, so a second edit of the same key overwrites the first
-- rather than queueing behind it (§3.5: "the draft holds what you want,
-- not the history of what you asked for"). `node_key` is the lowercase
-- DevEUI, the same convention `field_thresholds`/`device_meta` use for a
-- Node's `entity`.
--
-- `kind` is an addition to §10's column list, and it is what lets one
-- table hold both halves of the draft. The two halves do not send the same
-- way: a config key rides `set_sticker_config` on its own, while thresholds
-- ride `set_lorawan_field_threshold`, which takes **all four bounds of one
-- field at once**. Threshold rows are therefore keyed
-- `threshold.{field}.{bound}` and regrouped per field by the send walk,
-- which fills any bound the operator did not touch from the merged current
-- thresholds. Config rows carry the canonical dotted key
-- (`application.interval_sample`, `application.interval_report`) validated
-- by `fiber_protocol::STICKER_SETTABLE`. Without `kind` the walk would have
-- to infer the wire verb from the shape of the key string, which is the
-- kind of parsing that silently does the wrong thing the day a config key
-- is named `threshold_something`.
--
-- `desired_value` is a JSON scalar as text, because the config half is
-- typed by `STICKER_SETTABLE` (`Bool`, `UintRange`, `HexStr`, `Enum`) and
-- the threshold half is a nullable float — one TEXT column holding
-- `serde_json` output round-trips both, and a NULL bound (`null`) stays
-- distinguishable from an unset one (no row).
--
-- `state` is the send machine: `pending` (drafted, nothing sent),
-- `sending` (relayed, waiting for the device to confirm), `error` (the
-- relay refused, or the device did not confirm inside its budget). A
-- confirmed unit is DELETED rather than moved to a fourth state — the
-- draft is what is still outstanding, and a row that has landed on the
-- device is no longer outstanding. `'confirmed'` is nonetheless in the
-- CHECK: §7's column list names it, a future two-phase send may want to
-- park a unit there, and widening a CHECK later costs a table rebuild while
-- allowing one unused value costs nothing.
--
-- `author` is the username that last wrote the row, denormalised exactly as
-- `field_thresholds.actor` and `alarms.acknowledged_by` are, and for the
-- same reason: the draft banner must still say "changed by X" after that
-- user is deleted. No FK to `users`.
--
-- `attempts` and `last_error` are per unit, not per draft: a walk stops at
-- the first failure, so the row that failed is the one that has to explain
-- itself when the banner is re-rendered.
CREATE TABLE pending_config (
    hostname      TEXT    NOT NULL,
    node_key      TEXT    NOT NULL,
    param_key     TEXT    NOT NULL,
    kind          TEXT    NOT NULL CHECK (kind IN ('config', 'threshold')),
    desired_value TEXT,
    author        TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    state         TEXT    NOT NULL DEFAULT 'pending'
                          CHECK (state IN ('pending', 'sending', 'confirmed', 'error')),
    attempts      INTEGER NOT NULL DEFAULT 0,
    last_error    TEXT,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (hostname, node_key, param_key)
);

-- The `/nodes` list asks "does this Node have a draft" for every row in one
-- query, and the detail page and the send walk both read one Node's whole
-- draft. Both are prefix reads on the primary key, which SQLite serves from
-- the implicit index on it — no secondary index is added here, deliberately:
-- there is no query that filters on `state` or `author` alone.
