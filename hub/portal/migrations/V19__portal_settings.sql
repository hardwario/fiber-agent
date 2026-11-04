-- Installation-wide display settings (plan/decisions.md #19). A singleton,
-- like `notify_global_config`: one portal, one answer.
--
-- `display_timezone` — the IANA zone every instant is shown in. Stored
-- instants stay UTC; this is applied only when one becomes text for a person.
-- 'Europe/Prague' by default, the zone the product ships into; an admin
-- changes it on the settings page.
CREATE TABLE portal_settings (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    display_timezone  TEXT NOT NULL DEFAULT 'Europe/Prague',
    updated_at        TEXT
);

INSERT INTO portal_settings (id) VALUES (1);
