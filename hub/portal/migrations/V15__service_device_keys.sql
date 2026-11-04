-- The portal's own signing identities, separate from users'.
--
-- plan/portal/mqtt/design.md §5.4: two publish sites have no user behind
-- them. The alarm ack does have one — the acking user signs it — but
-- backfill does not: it is a background loop, and there is no one to
-- attribute its `get_sticker_history` to. It gets a **portal service
-- signer** instead, one Ed25519 key minted under the portal-held device CA
-- with exactly `get_status` and `level:read`, never usable for anything
-- above `read`. The device trusts it through the same CA it already trusts,
-- and the audit rows carry `signer_id: portal-service` rather than an
-- absence.
--
-- It cannot live in `user_device_keys`: that table's `user_id` is a foreign
-- key into `users`, and a service signer is not a user. A sentinel row in
-- `users` would be worse — it would appear in every user list, every
-- permission screen and every "who acknowledged this" query.
--
-- Same column shape as `user_device_keys` otherwise, keyed on the signer id
-- instead, so `KeyService` reads and unwraps it through the same code path.
CREATE TABLE service_device_keys (
    signer_id      TEXT    NOT NULL,
    hostname       TEXT    NOT NULL,
    seed_enc       BLOB    NOT NULL,
    public_key_hex TEXT    NOT NULL,
    fingerprint    TEXT    NOT NULL,
    certificate    TEXT    NOT NULL,
    permissions    TEXT    NOT NULL,
    created_at     TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (signer_id, hostname)
);
