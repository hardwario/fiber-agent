-- A Node's configuration that reached the Hub and the portal but not the Node
-- (plan/portal/nodes_phase1.cs.md, dodatek 2026-09-27, PF-3).
--
-- Since PF-3 the send walk writes a threshold to the Hub and to
-- `field_thresholds` first and only then sends the Node its part. A Node unit
-- the Node does not confirm in time is therefore no longer a plain `error`:
-- the Hub and the portal already hold the new bound, the Node the old one,
-- and "Send again" must send the Node its part only. Two things V13 cannot
-- hold, and this adds both (1, 2), plus the per-field record of what the Node
-- itself has confirmed (3).
--
-- **1. `state = 'undelivered'`.** SQLite cannot widen a CHECK in place, so
-- this is the table rebuild V16 is the precedent for. The row keeps its value
-- and its reason (`last_error`); the new state is what the banner renders as
-- "not delivered, send again" and what `pending::plan_send` reads as "the Hub
-- already has this".
--
-- **2. `previous_value`**: what the Hub and the portal held for this bound
-- before the send wrote it, as the same JSON scalar text `desired_value` uses
-- (`null` for an unset bound). Written once, when the Hub accepts the new
-- bound, and never overwritten by a resend or an edit: it is what a Discard
-- writes back to the Hub and to `field_thresholds`, and what the banner shows
-- as the value on the device. SQL NULL = the send never reached the Hub for
-- this row (every config key, every row of a fresh draft).
--
-- `previous_band` beside it: the whole band of the row's field before that
-- send (all four bounds and the switch, as JSON), written at the same moment
-- and just as once. A Discard puts the field back to it as a whole: the Hub
-- took the warning bounds in the same write, and their own draft rows are gone
-- by then, so reverting bound by bound would leave half a send (and could
-- invert a band). `previous_order` numbers the landings: a field's revert
-- point is the band of its oldest row still in the draft, so a second send
-- that only added a bound does not move it, and a late ack that finishes the
-- first send's rows moves it on to the band after that send.
--
-- **3. `node_threshold_delivery`**: per (Hub, Node, field) of the Node's alarm
-- table, the critical band and switch the Node last confirmed and when, and
-- whether a newer one is on its way (`pending`) or did not land in time
-- (`undelivered`). The thresholds table shows it as "on the Node: yes, <time>"
-- or "no, not delivered" (decision Hynek 2026-09-27). The portal reads no alarm
-- slots back from the Node (WP2), so this is the only record of what the Node
-- holds: written when the Node acks a table, never guessed. No row = the
-- portal has not seen the Node confirm a rule for that field. Only the
-- critical band lives on the Node; warning bounds never get a row.
--
-- **The number.** On its branch and on the bench Hubs (since c51) this was
-- V19. `michal/portal-product-review` reached `main` first with V17..V23, so
-- it lands as V25, above them, with V24, V26 and V27 of the same branch
-- (2026-09-30). A bench database that applied the branch numbers does not
-- open on this tree (refinery's `abort_divergent`) and is recreated.
--
-- `pending_config` carries no index, view or trigger, and nothing references
-- it by foreign key, so the rebuild is these statements and nothing else.

CREATE TABLE pending_config_v25 (
    hostname       TEXT    NOT NULL,
    node_key       TEXT    NOT NULL,
    param_key      TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('config', 'threshold')),
    desired_value  TEXT,
    author         TEXT    NOT NULL,
    created_at     INTEGER NOT NULL,
    state          TEXT    NOT NULL DEFAULT 'pending'
                           CHECK (state IN ('pending', 'sending', 'confirmed', 'error',
                                            'undelivered')),
    attempts       INTEGER NOT NULL DEFAULT 0,
    last_error     TEXT,
    updated_at     INTEGER NOT NULL,
    previous_value TEXT,
    previous_band  TEXT,
    previous_order INTEGER,
    PRIMARY KEY (hostname, node_key, param_key)
);

INSERT INTO pending_config_v25
    (hostname, node_key, param_key, kind, desired_value, author, created_at,
     state, attempts, last_error, updated_at, previous_value, previous_band, previous_order)
SELECT
    hostname, node_key, param_key, kind, desired_value, author, created_at,
    state, attempts, last_error, updated_at, NULL, NULL, NULL
FROM pending_config;

DROP TABLE pending_config;

ALTER TABLE pending_config_v25 RENAME TO pending_config;

CREATE TABLE node_threshold_delivery (
    hostname          TEXT    NOT NULL,
    node_key          TEXT    NOT NULL,
    field             TEXT    NOT NULL,
    -- The band the Node last confirmed. NULL before its first confirmation,
    -- which a row in `pending` or `undelivered` can be.
    confirmed_low     REAL,
    confirmed_high    REAL,
    confirmed_enabled INTEGER,
    confirmed_at      INTEGER,
    state             TEXT    NOT NULL
                              CHECK (state IN ('confirmed', 'pending', 'undelivered')),
    updated_at        INTEGER NOT NULL,
    PRIMARY KEY (hostname, node_key, field)
);
