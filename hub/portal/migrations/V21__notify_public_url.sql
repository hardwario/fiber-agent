-- The address e-mail recipients open the portal at (UX-027,
-- plan/portal/email_links.md §2).
--
-- `public_url` — an origin without a trailing slash
-- (`https://proximos-ce3d59f8.local`). Alarm e-mails link to
-- `{public_url}/alarms/{id}`, fault e-mails to `{public_url}/hubs/{hostname}`.
-- NULL by default: an e-mail then carries no link, never a guessed host. Set
-- by an admin on the notification settings page.
ALTER TABLE notify_global_config ADD COLUMN public_url TEXT;
