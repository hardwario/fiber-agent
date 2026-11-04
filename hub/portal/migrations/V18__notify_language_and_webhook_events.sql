-- Two notification settings the channels were missing (plan/decisions.md #20).
--
-- `email_lang` — the language alarm and system-alert e-mails are written in.
-- The portal UI speaks cs and en per viewer; an e-mail has no viewer, so the
-- installation picks one. 'cs' by default: until now every e-mail was English
-- whatever the customer read.
--
-- `webhook_lifecycle` — whether the webhook also fires on escalation's
-- siblings, clear and acknowledge (`alarm.cleared`, `alarm.acknowledged`).
-- Off by default: a receiver written against v1 only ever saw
-- `alarm.activated`, and one that does not read `event` would take a clear
-- for a new alarm. `alarm.escalated` is not behind it — it is an alarm.
ALTER TABLE notify_global_config ADD COLUMN email_lang TEXT NOT NULL DEFAULT 'cs'
    CHECK (email_lang IN ('cs', 'en'));
ALTER TABLE notify_global_config ADD COLUMN webhook_lifecycle INTEGER NOT NULL DEFAULT 0;
