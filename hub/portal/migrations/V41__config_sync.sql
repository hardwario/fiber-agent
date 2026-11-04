-- Portal -> Hub configuration sync, Portal side of the Hub pull
-- (plan/portal/config_sync.md §5, §9; decision #43).
--
-- One `config_generation` row per Hub: `gen` grows on every save of a
-- desired verb touching the Hub or its Beacons/Nodes; `hash` is the
-- `fiber_protocol::config_sync::document_hash` of the desired document at
-- that generation. The `acked_*` / `last_req_at` columns are what the Hub
-- reported and feed the one `sync:<hub>` alarm per Hub (§10, Q12).
--
-- `config_sync_item` keeps, per document key, who saved it last (the
-- editor the item is re-signed with at send time, §6) and the outcome of
-- the Hub's last ack for it. §9's per-item UI states were dropped by the
-- Q12 amendment, so `state` only distinguishes failed from the rest;
-- `failed_since` drives the "failed item > 1 h" alarm rule.
-- Unix seconds throughout. `hub_id` is the lowercase hostname.
CREATE TABLE config_generation (
    hub_id       TEXT    PRIMARY KEY,
    gen          INTEGER NOT NULL,
    hash         TEXT    NOT NULL,
    changed_at   INTEGER NOT NULL,
    acked_gen    INTEGER,
    acked_hash   TEXT,
    acked_at     INTEGER,
    last_req_at  INTEGER
);

CREATE TABLE config_sync_item (
    hub_id       TEXT    NOT NULL,
    key          TEXT    NOT NULL,
    gen          INTEGER NOT NULL,
    editor       TEXT,
    state        TEXT    NOT NULL DEFAULT 'pending'
                 CHECK (state IN ('pending', 'synced', 'failed')),
    last_error   TEXT,
    failed_since INTEGER,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (hub_id, key)
);
