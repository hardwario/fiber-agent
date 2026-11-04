-- What the Node holds after a Discard that took back a frame it may already have had
-- (plan/portal/nodes_phase1.cs.md, dodatek 2026-09-27, PF-3b).
--
-- On the bench (c51) a muted Node "received" a 0x56 into its deaf RX1, the P2P central kept that
-- delivered head, and after a Discard reverted the Hub and the portal it re-delivered the
-- discarded bound three uplinks later. A Discard now takes such a frame back too. The Hub cannot
-- know whether the Node applied it, so V25's three states are not enough:
--
-- * `unknown` — a frame of the discarded send had reached the Node, or may have: it may hold
--   the discarded band or the one before. The thresholds table says so instead of quoting the
--   last confirmation's time as if nothing had happened.
-- * `differs` — the Node's late answer to a discarded frame proved it took it: it holds `sent_*`,
--   a band the portal no longer holds. An active configuration warning stays open until a
--   confirmed table replaces it.
--
-- `sent_low`, `sent_high`, `sent_enabled`: the band last sent to the Node and not yet
-- confirmed, written with `pending`, for `differs` to say what the Node holds.
--
-- V25 (V19 on the bench Hubs since c51) had shipped, so this is a new version and not an edit of
-- it; it was V20 on its branch and follows V25 (V25's header has the renumbering). The table
-- carries no index, view or trigger and nothing references it, so the rebuild is these
-- statements.

CREATE TABLE node_threshold_delivery_v26 (
    hostname          TEXT    NOT NULL,
    node_key          TEXT    NOT NULL,
    field             TEXT    NOT NULL,
    confirmed_low     REAL,
    confirmed_high    REAL,
    confirmed_enabled INTEGER,
    confirmed_at      INTEGER,
    state             TEXT    NOT NULL
                              CHECK (state IN ('confirmed', 'pending', 'undelivered',
                                               'unknown', 'differs')),
    updated_at        INTEGER NOT NULL,
    sent_low          REAL,
    sent_high         REAL,
    sent_enabled      INTEGER,
    PRIMARY KEY (hostname, node_key, field)
);

INSERT INTO node_threshold_delivery_v26
    (hostname, node_key, field, confirmed_low, confirmed_high, confirmed_enabled,
     confirmed_at, state, updated_at, sent_low, sent_high, sent_enabled)
SELECT
    hostname, node_key, field, confirmed_low, confirmed_high, confirmed_enabled,
    confirmed_at, state, updated_at, NULL, NULL, NULL
FROM node_threshold_delivery;

DROP TABLE node_threshold_delivery;

ALTER TABLE node_threshold_delivery_v26 RENAME TO node_threshold_delivery;
