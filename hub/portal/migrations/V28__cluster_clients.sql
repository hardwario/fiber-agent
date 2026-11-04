-- The Server's registry of Client Hubs (plan/portal/devices.hub.md §1.2, §4, §5,
-- §12 J; plan/control/cluster.md §7, §9; milestone C5).
--
-- A Client Hub is known to this portal from the moment an admin registers it,
-- before it has ever connected: the row is what gives it a Hubs-list row and a
-- detail page while the store has nothing for it (`known_hub()` = store ∪ this
-- table). The Server's agent holds the account itself (`cluster-client-add`,
-- C2); this table holds what the portal needs to show and manage it.
--
-- **No password column, by design.** The agent mints a 22-character broker
-- password and returns it once in `config/response.result`; the portal shows
-- it once, in the join bundle, and keeps nothing. A lost password is a re-run
-- of Add hub, which rotates it (cluster.md §7, devices.hub.md §8.4).
--
-- `registration_state` (cluster.md §7):
--   pending  — both effects good (account ok, gateway ok or skipped); the
--              Client has not yet appeared on this Server's broker.
--   failed   — "registration incomplete": the account or the gateway failed,
--              or no answer came. Never a silent half; Retry re-runs both.
--   complete — the Client's agent `status` reached this broker (§9 liveness).
-- A row is inserted as `failed` with "did not finish" before the command is
-- sent and moved on by the answer, so a portal that stops mid-command leaves
-- an honest row behind and two admins racing on one hostname or EUI collide
-- on the keys below instead of both minting.
--
-- `removal_error` is set when `cluster-client-remove` did not complete
-- (devices.hub.md §5: "odebrání nedokončeno" with Retry); a removal that
-- completes deletes the row. NULL = no failed removal.
--
-- The hostname is decision #17's `proximos-<8 hex>`, lowercase (#21); the
-- CHECK keeps a hand-edited row from breaking the topic segment it becomes.
-- The EUI is 16 lowercase hex or NULL (a Client without a LoRaWAN gateway),
-- unique so one gateway is never registered to two Clients.

CREATE TABLE cluster_clients (
    hostname            TEXT PRIMARY KEY
                        CHECK (length(hostname) = 17
                               AND hostname GLOB 'proximos-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]'),
    gateway_eui         TEXT UNIQUE
                        CHECK (gateway_eui IS NULL
                               OR (length(gateway_eui) = 16 AND gateway_eui NOT GLOB '*[^0-9a-f]*')),
    peer_username       TEXT NOT NULL,
    registration_state  TEXT NOT NULL
                        CHECK (registration_state IN ('pending', 'failed', 'complete')),
    registration_error  TEXT,
    removal_error       TEXT,
    registered_at       TEXT NOT NULL DEFAULT (datetime('now')),
    registered_by       TEXT,
    last_attempt_at     TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at        TEXT
);
